use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing_futures::Instrument;

use crate::core::CandidateState;
use crate::core::RaftCore;
use crate::core::State;
use crate::error::VoteError;
use crate::raft::VoteRequest;
use crate::raft::VoteResponse;
use crate::summary::MessageSummary;
use crate::AppData;
use crate::AppDataResponse;
use crate::NodeId;
use crate::RaftNetwork;
use crate::RaftStorage;
use crate::StorageError;

impl<D: AppData, R: AppDataResponse, N: RaftNetwork<D>, S: RaftStorage<D, R>> RaftCore<D, R, N, S> {
    /// An RPC invoked by candidates to gather votes (§5.2).
    ///
    /// See `receiver implementation: RequestVote RPC` in raft-essentials.md in this repo.
    #[tracing::instrument(level = "debug", skip(self, req), fields(req=%req.summary()))]
    pub(super) async fn handle_vote_request(&mut self, req: VoteRequest) -> Result<VoteResponse, VoteError> {
        tracing::debug!(
            %req.vote,
            ?self.vote,
            "start handle_vote_request"
        );
        let last_log_id = self.last_log_id;

        if req.vote < self.vote {
            tracing::debug!(
                %req.vote,
                ?self.vote,
                "RequestVote RPC term is less than current term"
            );
            return Ok(VoteResponse {
                vote: self.vote,
                vote_granted: false,
                last_log_id,
            });
        }

        // Do not respond to the request if we've received a heartbeat within the election timeout minimum.
        if let Some(inst) = &self.last_heartbeat {
            let now = Instant::now();
            let delta = now.duration_since(*inst);
            if self.config.election_timeout_min >= (delta.as_millis() as u64) {
                tracing::debug!(
                    %req.vote,
                    ?delta,
                    "rejecting vote request received within election timeout minimum"
                );
                return Ok(VoteResponse {
                    vote: self.vote,
                    vote_granted: false,
                    last_log_id,
                });
            }
        }

        // Check if candidate's log is at least as up-to-date as this node's.
        // If candidate's log is not at least as up-to-date as this node, then reject.
        if req.last_log_id < last_log_id {
            tracing::debug!(
                %req.vote,
                "rejecting vote request as candidate's log is not up-to-date"
            );
            return Ok(VoteResponse {
                vote: self.vote,
                vote_granted: false,
                last_log_id,
            });
        }

        self.update_next_election_timeout(false);
        self.vote = req.vote;
        self.save_vote().await?;

        self.set_target_state(State::Follower);

        tracing::debug!(%req.vote, "voted for candidate");

        Ok(VoteResponse {
            vote: self.vote,
            vote_granted: true,
            last_log_id,
        })
    }
}

impl<'a, D: AppData, R: AppDataResponse, N: RaftNetwork<D>, S: RaftStorage<D, R>> CandidateState<'a, D, R, N, S> {
    /// Handle response from a vote request sent to a peer.
    #[tracing::instrument(level = "debug", skip(self, res))]
    pub(super) async fn handle_vote_response(&mut self, res: VoteResponse, target: NodeId) -> Result<(), StorageError> {
        tracing::debug!(res=?res, target, "recv vote response");

        // If peer's vote is greater than current vote, revert to follower state.

        if res.vote > self.core.vote {
            // If the core.vote is changed(to some greater value), then no further vote response would be valid.
            // Because they just granted an old `vote`.
            // A quorum does not mean the core is legal to use the new greater `vote`.
            // Thus no matter the last_log_id is greater than the remote peer or not, revert to follower at once.

            // TODO(xp): This is a simplified impl: revert to follower as soon as seeing a higher `last_log_id`.
            //           When reverted to follower, it waits for heartbeat for 2 second before starting a new round of
            //           election.
            self.core.set_target_state(State::Follower);

            tracing::debug!(
                id = %self.core.id,
                %res.vote,
                %self.core.vote,
                self_last_log_id=?self.core.last_log_id,
                res_last_log_id=?res.last_log_id,
                "reverting to follower state due to greater vote observed in RequestVote RPC response");

            self.core.vote = res.vote;
            self.core.save_vote().await?;

            return Ok(());
        }

        if res.vote_granted {
            self.granted.insert(target);

            if self.core.effective_membership.membership.is_majority(&self.granted) {
                tracing::debug!("transitioning to leader state as minimum number of votes have been received");
                self.core.set_target_state(State::Leader);
                return Ok(());
            }
        }

        // Otherwise, we just return and let the candidate loop wait for more votes to come in.
        Ok(())
    }

    /// Spawn parallel vote requests to all cluster members.
    #[tracing::instrument(level = "trace", skip(self))]
    pub(super) fn spawn_parallel_vote_requests(&self) -> mpsc::Receiver<(VoteResponse, NodeId)> {
        let all_nodes = self.core.effective_membership.membership.all_members().clone();
        let (tx, rx) = mpsc::channel(all_nodes.len());

        for member in all_nodes.into_iter().filter(|member| member != &self.core.id) {
            let rpc = VoteRequest::new(self.core.vote, self.core.last_log_id);

            let (network, tx_inner) = (self.core.network.clone(), tx.clone());
            let _ = tokio::spawn(
                async move {
                    let res = network.send_vote(member, rpc).await;

                    match res {
                        Ok(vote_resp) => {
                            let _ = tx_inner.send((vote_resp, member)).await;
                        }
                        Err(err) => tracing::error!({error=%err, target=member}, "while requesting vote"),
                    }
                }
                .instrument(tracing::debug_span!("send_vote_req", target = member)),
            );
        }
        rx
    }
}
