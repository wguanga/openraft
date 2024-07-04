use std::time::Duration;

use validit::Valid;

use crate::core::raft_msg::AppendEntriesTx;
use crate::core::raft_msg::ResultSender;
use crate::core::sm;
use crate::core::ServerState;
use crate::display_ext::DisplayInstantExt;
use crate::display_ext::DisplayOptionExt;
use crate::display_ext::DisplaySlice;
use crate::engine::engine_config::EngineConfig;
use crate::engine::handler::establish_handler::EstablishHandler;
use crate::engine::handler::following_handler::FollowingHandler;
use crate::engine::handler::leader_handler::LeaderHandler;
use crate::engine::handler::log_handler::LogHandler;
use crate::engine::handler::replication_handler::ReplicationHandler;
use crate::engine::handler::replication_handler::SendNone;
use crate::engine::handler::server_state_handler::ServerStateHandler;
use crate::engine::handler::snapshot_handler::SnapshotHandler;
use crate::engine::handler::vote_handler::VoteHandler;
use crate::engine::Command;
use crate::engine::EngineOutput;
use crate::engine::Respond;
use crate::entry::RaftEntry;
use crate::entry::RaftPayload;
use crate::error::ForwardToLeader;
use crate::error::Infallible;
use crate::error::InitializeError;
use crate::error::NotAllowed;
use crate::error::NotInMembers;
use crate::error::RejectAppendEntries;
use crate::proposer::leader_state::CandidateState;
use crate::proposer::Candidate;
use crate::proposer::LeaderQuorumSet;
use crate::proposer::LeaderState;
use crate::raft::responder::Responder;
use crate::raft::AppendEntriesResponse;
use crate::raft::SnapshotResponse;
use crate::raft::VoteRequest;
use crate::raft::VoteResponse;
use crate::raft_state::LogStateReader;
use crate::raft_state::RaftState;
use crate::type_config::alias::InstantOf;
use crate::type_config::alias::ResponderOf;
use crate::type_config::alias::SnapshotDataOf;
use crate::Instant;
use crate::LogId;
use crate::LogIdOptionExt;
use crate::Membership;
use crate::RaftLogId;
use crate::RaftTypeConfig;
use crate::Snapshot;
use crate::SnapshotMeta;
use crate::Vote;

/// Raft protocol algorithm.
///
/// It implement the complete raft algorithm except does not actually update any states.
/// But instead, it output commands to let a `RaftRuntime` implementation execute them to actually
/// update the states such as append-log or save-vote by execute .
///
/// This structure only contains necessary information to run raft algorithm,
/// but none of the application specific data.
/// TODO: make the fields private
#[derive(Debug)]
pub(crate) struct Engine<C>
where C: RaftTypeConfig
{
    pub(crate) config: EngineConfig<C>,

    /// The state of this raft node.
    pub(crate) state: Valid<RaftState<C>>,

    // TODO: add a Voting state as a container.
    /// Whether a greater log id is seen during election.
    ///
    /// If it is true, then this node **may** not become a leader therefore the election timeout
    /// should be greater.
    pub(crate) seen_greater_log: bool,

    /// The greatest vote this node has ever seen.
    ///
    /// It could be greater than `self.state.vote`,
    /// because `self.state.vote` is update only when a node granted a vote,
    /// i.e., the Leader with this vote is legal: has a greater log and vote.
    ///
    /// This vote value is used for election.
    pub(crate) last_seen_vote: Vote<C::NodeId>,

    /// Represents the Leader state.
    pub(crate) leader: LeaderState<C>,

    /// Represents the Candidate state within Openraft.
    ///
    /// A Candidate can coexist with a Leader in the system.
    /// This scenario is typically used to transition the Leader to a higher term (vote)
    /// without losing leadership status.
    pub(crate) candidate: CandidateState<C>,

    /// Output entry for the runtime.
    pub(crate) output: EngineOutput<C>,
}

impl<C> Engine<C>
where C: RaftTypeConfig
{
    pub(crate) fn new(init_state: RaftState<C>, config: EngineConfig<C>) -> Self {
        let vote = *init_state.vote_ref();
        Self {
            config,
            state: Valid::new(init_state),
            seen_greater_log: false,
            last_seen_vote: vote,
            leader: None,
            candidate: None,
            output: EngineOutput::new(4096),
        }
    }

    /// Create a new candidate state and return the mutable reference to it.
    ///
    /// The candidate `last_log_id` is initialized with the attributes of Acceptor part:
    /// [`RaftState`]
    pub(crate) fn new_candidate(&mut self, vote: Vote<C::NodeId>) -> &mut Candidate<C, LeaderQuorumSet<C::NodeId>> {
        let now = InstantOf::<C>::now();
        let last_log_id = self.state.last_log_id().copied();

        let membership = self.state.membership_state.effective().membership();

        self.candidate = Some(Candidate::new(
            now,
            vote,
            last_log_id,
            membership.to_quorum_set(),
            membership.learner_ids(),
        ));

        self.candidate.as_mut().unwrap()
    }

    /// Create a default Engine for testing.
    #[allow(dead_code)]
    pub(crate) fn testing_default(id: C::NodeId) -> Self {
        let config = EngineConfig::new_default(id);
        let state = RaftState::default();
        Self::new(state, config)
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) fn startup(&mut self) {
        // Allows starting up as a leader.

        tracing::info!(
            "startup begin: state: {:?}, is_leader: {}, is_voter: {}",
            self.state,
            self.state.is_leader(&self.config.id),
            self.state.membership_state.effective().is_voter(&self.config.id)
        );

        // Previously it is a leader. restore it as leader at once
        if self.state.is_leader(&self.config.id) {
            self.vote_handler().update_internal_server_state();

            let mut rh = self.replication_handler();

            // Restore the progress about the local log
            rh.update_local_progress(rh.state.last_log_id().copied());

            rh.initiate_replication(SendNone::False);

            return;
        }

        let server_state = if self.state.membership_state.effective().is_voter(&self.config.id) {
            ServerState::Follower
        } else {
            ServerState::Learner
        };

        self.state.server_state = server_state;

        tracing::info!(
            "startup done: id={} target_state: {:?}",
            self.config.id,
            self.state.server_state
        );
    }

    /// Initialize a node by appending the first log.
    ///
    /// - The first log has to be membership config log.
    /// - The node has to contain no logs at all and the vote is the minimal value. See: [Conditions
    ///   for initialization][precondition].
    ///
    ///
    /// Appending the very first log is slightly different from appending log by a leader or
    /// follower. This step is not confined by the consensus protocol and has to be dealt with
    /// differently.
    ///
    /// [precondition]: crate::docs::cluster_control::cluster_formation#preconditions-for-initialization
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) fn initialize(&mut self, mut entry: C::Entry) -> Result<(), InitializeError<C>> {
        self.check_initialize()?;

        // The very first log id
        entry.set_log_id(&LogId::default());

        let m = entry.get_membership().expect("the only log entry for initializing has to be membership log");
        self.check_members_contain_me(m)?;

        self.following_handler().do_append_entries(vec![entry], 0);

        // With the new config, start to elect to become leader
        self.elect();

        Ok(())
    }

    /// Start to elect this node as leader
    #[tracing::instrument(level = "debug", skip(self))]
    pub(crate) fn elect(&mut self) {
        debug_assert!(
            self.last_seen_vote >= *self.state.vote_ref(),
            "expect: last_seen_vote({}) >= state.vote({}), when elect()",
            self.last_seen_vote,
            self.state.vote_ref()
        );

        let new_term = self.last_seen_vote.leader_id().term + 1;
        let new_vote = Vote::new(new_term, self.config.id);

        let candidate = self.new_candidate(new_vote);

        tracing::info!("{}, new candidate: {}", func_name!(), candidate);

        let last_log_id = candidate.last_log_id().copied();

        // Simulate sending RequestVote RPC to local node.
        // Safe unwrap(): it won't reject itself ˙–˙
        self.vote_handler().update_vote(&new_vote).unwrap();

        self.output.push_command(Command::SendVote {
            vote_req: VoteRequest::new(new_vote, last_log_id),
        });

        self.server_state_handler().update_server_state_if_changed();
    }

    pub(crate) fn candidate_ref(&self) -> Option<&Candidate<C, LeaderQuorumSet<C::NodeId>>> {
        self.candidate.as_ref()
    }

    pub(crate) fn candidate_mut(&mut self) -> Option<&mut Candidate<C, LeaderQuorumSet<C::NodeId>>> {
        self.candidate.as_mut()
    }

    /// Get a LeaderHandler for handling leader's operation. If it is not a leader, it send back a
    /// ForwardToLeader error through the tx.
    ///
    /// If tx is None, no response will be sent.
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) fn get_leader_handler_or_reject(
        &mut self,
        tx: Option<ResponderOf<C>>,
    ) -> Option<(LeaderHandler<C>, Option<ResponderOf<C>>)> {
        let res = self.leader_handler();
        let forward_err = match res {
            Ok(lh) => {
                tracing::debug!("this node is a leader");
                return Some((lh, tx));
            }
            Err(forward_err) => forward_err,
        };

        if let Some(tx) = tx {
            tx.send(Err(forward_err.into()));
        }

        None
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) fn handle_vote_req(&mut self, req: VoteRequest<C>) -> VoteResponse<C> {
        let now = InstantOf::<C>::now();
        let lease = self.config.timer_config.leader_lease;
        let vote = self.state.vote_ref();

        // Make default vote-last-modified a low enough value, that expires leader lease.
        let vote_utime = self.state.vote_last_modified().unwrap_or_else(|| now - lease - Duration::from_millis(1));

        tracing::info!(req = display(&req), "Engine::handle_vote_req");
        tracing::info!(
            my_vote = display(self.state.vote_ref()),
            my_last_log_id = display(self.state.last_log_id().display()),
            "Engine::handle_vote_req"
        );
        tracing::info!(
            "now; {}, vote is updated at: {}, vote is updated before {:?}, leader lease({:?}) will expire after {:?}",
            now.display(),
            vote_utime.display(),
            now - vote_utime,
            lease,
            vote_utime + lease - now
        );

        if vote.is_committed() {
            // Current leader lease has not yet expired, reject voting request
            if now <= vote_utime + lease {
                tracing::info!(
                    "reject vote-request: leader lease has not yet expire; now; {:?}, vote is updatd at: {:?}, leader lease({:?}) will expire after {:?}",
                    now,
                    vote_utime,
                    lease,
                    vote_utime + lease - now
                );

                return VoteResponse::new(self.state.vote_ref(), self.state.last_log_id().copied());
            }
        }

        // The first step is to check log. If the candidate has less log, nothing needs to be done.

        if req.last_log_id.as_ref() >= self.state.last_log_id() {
            // Ok
        } else {
            tracing::info!(
                "reject vote-request: by last_log_id: !(req.last_log_id({}) >= my_last_log_id({})",
                req.last_log_id.display(),
                self.state.last_log_id().display(),
            );
            // The res is not used yet.
            // let _res = Err(RejectVoteRequest::ByLastLogId(self.state.last_log_id().copied()));

            // Return the updated vote, this way the candidate knows which vote is granted, in case
            // the candidate's vote is changed after sending the vote request.
            return VoteResponse::new(self.state.vote_ref(), self.state.last_log_id().copied());
        }

        // Then check vote just as it does for every incoming event.

        let res = self.vote_handler().update_vote(&req.vote);

        tracing::info!(req = display(&req), result = debug(&res), "handle vote request result");

        // Return the updated vote, this way the candidate knows which vote is granted, in case
        // the candidate's vote is changed after sending the vote request.
        VoteResponse::new(self.state.vote_ref(), self.state.last_log_id().copied())
    }

    #[tracing::instrument(level = "debug", skip(self, resp))]
    pub(crate) fn handle_vote_resp(&mut self, target: C::NodeId, resp: VoteResponse<C>) {
        tracing::info!(
            resp = display(&resp),
            target = display(target),
            my_vote = display(self.state.vote_ref()),
            my_last_log_id = display(self.state.last_log_id().display()),
            "{}",
            func_name!()
        );

        // Update the last seen vote, but not `state.vote`.
        // `state.vote` is updated only when the vote is granted
        // (allows the vote owner to be a Leader).
        //
        // But in this case, the responded greater vote is not yet granted
        // because the remote peer may have smaller log.
        // And even when the remote peer has greater log, it does not have to grant the vote,
        // if greater logs does not form a quorum.
        self.vote_handler().update_last_seen(&resp.vote);

        let Some(candidate) = self.candidate_mut() else {
            // If the voting process has finished or canceled,
            // just ignore the delayed vote_resp.
            return;
        };

        // A vote request is granted iff the replied vote is the same as the requested vote.
        if &resp.vote == candidate.vote_ref() {
            let quorum_granted = candidate.grant_by(&target);
            if quorum_granted {
                tracing::info!("a quorum granted my vote");
                self.establish_leader();
            }
            return;
        }

        // TODO: resp.granted is never used.

        // If not equal, vote is rejected:

        // Note that it is still possible seeing a smaller vote:
        // - The target has more logs than this node;
        // - Or leader lease on remote node is not expired;
        // - It is a delayed response of previous voting(resp.vote_granted could be true)
        // In any case, no need to proceed.

        // Seen a higher log. Record it so that the next election will be delayed for a while.
        if resp.last_log_id.as_ref() > self.state.last_log_id() {
            tracing::info!(
                greater_log_id = display(resp.last_log_id.display()),
                "seen a greater log id when {}",
                func_name!()
            );
            self.set_greater_log();
        }
    }

    /// Append entries to follower/learner.
    ///
    /// Also clean conflicting entries and update membership state.
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) fn handle_append_entries(
        &mut self,
        vote: &Vote<C::NodeId>,
        prev_log_id: Option<LogId<C::NodeId>>,
        entries: Vec<C::Entry>,
        tx: Option<AppendEntriesTx<C>>,
    ) -> bool {
        tracing::debug!(
            vote = display(vote),
            prev_log_id = display(prev_log_id.display()),
            entries = display(DisplaySlice::<_>(&entries)),
            my_vote = display(self.state.vote_ref()),
            my_last_log_id = display(self.state.last_log_id().display()),
            "{}",
            func_name!()
        );

        let res = self.append_entries(vote, prev_log_id, entries);
        let is_ok = res.is_ok();

        if let Some(tx) = tx {
            let resp: AppendEntriesResponse<C> = res.into();
            self.output.push_command(Command::Respond {
                when: None,
                resp: Respond::new(Ok(resp), tx),
            });
        }
        is_ok
    }

    pub(crate) fn append_entries(
        &mut self,
        vote: &Vote<C::NodeId>,
        prev_log_id: Option<LogId<C::NodeId>>,
        entries: Vec<C::Entry>,
    ) -> Result<(), RejectAppendEntries<C>> {
        self.vote_handler().update_vote(vote)?;

        // Vote is legal.

        let mut fh = self.following_handler();
        fh.ensure_log_consecutive(prev_log_id)?;
        fh.append_entries(prev_log_id, entries);

        Ok(())
    }

    /// Commit entries for follower/learner.
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) fn handle_commit_entries(&mut self, leader_committed: Option<LogId<C::NodeId>>) {
        tracing::debug!(
            leader_committed = display(leader_committed.display()),
            my_accepted = display(self.state.accepted().display()),
            my_committed = display(self.state.committed().display()),
            "{}",
            func_name!()
        );

        let mut fh = self.following_handler();
        fh.commit_entries(leader_committed);
    }

    /// Install a completely received snapshot on a follower.
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) fn handle_install_full_snapshot(
        &mut self,
        vote: Vote<C::NodeId>,
        snapshot: Snapshot<C>,
        tx: ResultSender<C, SnapshotResponse<C>>,
    ) {
        tracing::info!(vote = display(vote), snapshot = display(&snapshot), "{}", func_name!());

        let vote_res = self.vote_handler().accept_vote(&vote, tx, |state, _rejected| {
            Ok(SnapshotResponse::new(*state.vote_ref()))
        });

        let Some(tx) = vote_res else {
            return;
        };

        let mut fh = self.following_handler();

        // The condition to satisfy before running other command that depends on the snapshot.
        // In this case, the response can only be sent when the snapshot is installed.
        let cond = fh.install_full_snapshot(snapshot);
        let res = Ok(SnapshotResponse {
            vote: *self.state.vote_ref(),
        });

        self.output.push_command(Command::Respond {
            when: cond,
            resp: Respond::new(res, tx),
        });
    }

    /// Install a completely received snapshot on a follower.
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) fn handle_begin_receiving_snapshot(&mut self, tx: ResultSender<C, Box<SnapshotDataOf<C>>, Infallible>) {
        tracing::info!("{}", func_name!());
        self.output.push_command(Command::from(sm::Command::begin_receiving_snapshot(tx)));
    }

    /// Leader steps down(convert to learner) once the membership not containing it is committed.
    ///
    /// This is only called by leader.
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) fn leader_step_down(&mut self) {
        tracing::debug!("leader_step_down: node_id:{}", self.config.id);

        // Step down:
        // Keep acting as leader until a membership without this node is committed.
        let em = &self.state.membership_state.effective();

        tracing::debug!(
            "membership: {}, committed: {}, is_leading: {}",
            em,
            self.state.committed().display(),
            self.state.is_leading(&self.config.id),
        );

        #[allow(clippy::collapsible_if)]
        if em.log_id().as_ref() <= self.state.committed() {
            self.vote_handler().update_internal_server_state();
        }
    }

    /// Update Engine state when a new snapshot is built.
    ///
    /// NOTE:
    /// - Engine updates its state for building a snapshot is done after storage finished building a
    ///   snapshot,
    /// - while Engine updates its state for installing a snapshot is done before storage starts
    ///   installing a snapshot.
    ///
    /// This is all right because:
    /// - Engine only keeps the snapshot meta with the greatest last-log-id;
    /// - and a snapshot smaller than last-committed is not allowed to be installed.
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) fn finish_building_snapshot(&mut self, meta: SnapshotMeta<C>) {
        tracing::info!(snapshot_meta = display(&meta), "{}", func_name!());

        self.state.io_state_mut().set_building_snapshot(false);

        let mut h = self.snapshot_handler();

        let updated = h.update_snapshot(meta);
        if !updated {
            return;
        }

        self.log_handler().schedule_policy_based_purge();
        self.try_purge_log();
    }

    /// Try to purge logs up to the expected position.
    ///
    /// If the node is a leader, it will only purge logs when no replication tasks are using them.
    /// Otherwise, it will retry purging the logs the next time replication has made progress.
    ///
    /// If the node is a follower or learner, it will always purge the logs immediately since no
    /// other tasks are using the logs.
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) fn try_purge_log(&mut self) {
        tracing::debug!(
            purge_upto = display(self.state.purge_upto().display()),
            "{}",
            func_name!()
        );

        if self.leader.is_some() {
            // If it is leading, it must not delete a log that is in use by a replication task.
            self.replication_handler().try_purge_log();
        } else {
            // For follower/learner, no other tasks are using logs, just purge.
            self.log_handler().purge_log();
        }
    }

    /// This is a to user API that triggers log purging upto `index`, inclusive.
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) fn trigger_purge_log(&mut self, mut index: u64) {
        tracing::info!(index = display(index), "{}", func_name!());

        let snapshot_last_log_id = self.state.snapshot_last_log_id();
        let snapshot_last_log_id = if let Some(x) = snapshot_last_log_id {
            *x
        } else {
            tracing::info!("no snapshot, can not purge");
            return;
        };

        let scheduled = self.state.purge_upto();

        if index < scheduled.next_index() {
            tracing::info!(
                "no update, already scheduled: {}; index: {}",
                scheduled.display(),
                index,
            );
            return;
        }

        if index > snapshot_last_log_id.index {
            tracing::info!(
                "can not purge logs not in a snapshot; index: {}, last in snapshot log id: {}",
                index,
                snapshot_last_log_id
            );
            index = snapshot_last_log_id.index;
        }

        // Safe unwrap: `index` is ensured to be present in the above code.
        let log_id = self.state.get_log_id(index).unwrap();

        tracing::info!(purge_upto = display(log_id), "{}", func_name!());

        self.log_handler().update_purge_upto(log_id);
        self.try_purge_log();
    }
}

/// Supporting util
impl<C> Engine<C>
where C: RaftTypeConfig
{
    /// Vote is granted by a quorum, leader established.
    #[tracing::instrument(level = "debug", skip_all)]
    fn establish_leader(&mut self) {
        tracing::info!("{}", func_name!());

        let candidate = self.candidate.take().unwrap();
        let leader = self.establish_handler().establish(candidate);

        // There may already be a Leader with higher vote
        let Some(leader) = leader else { return };

        let vote = *leader.vote_ref();

        self.replication_handler().rebuild_replication_streams();

        // Before sending any log, update the vote.
        // This could not fail because `internal_server_state` will be cleared
        // once `state.vote` is changed to a value of other node.
        let _res = self.vote_handler().update_vote(&vote);
        debug_assert!(_res.is_ok(), "commit vote can not fail but: {:?}", _res);

        self.leader_handler()
            .unwrap()
            .leader_append_entries(vec![C::Entry::new_blank(LogId::<C::NodeId>::default())]);
    }

    /// Check if a raft node is in a state that allows to initialize.
    ///
    /// It is allowed to initialize only when `last_log_id.is_none()` and `vote==(term=0,
    /// node_id=0)`. See: [Conditions for initialization](https://datafuselabs.github.io/openraft/cluster-formation.html#conditions-for-initialization)
    fn check_initialize(&self) -> Result<(), NotAllowed<C>> {
        if !self.state.is_initialized() {
            return Ok(());
        }

        tracing::error!(
            last_log_id = display(self.state.last_log_id().display()),
            vote = display(self.state.vote_ref()),
            "Can not initialize"
        );

        Err(NotAllowed {
            last_log_id: self.state.last_log_id().copied(),
            vote: *self.state.vote_ref(),
        })
    }

    /// When initialize, the node that accept initialize request has to be a member of the initial
    /// config.
    fn check_members_contain_me(&self, m: &Membership<C>) -> Result<(), NotInMembers<C>> {
        if !m.is_voter(&self.config.id) {
            let e = NotInMembers {
                node_id: self.config.id,
                membership: m.clone(),
            };
            Err(e)
        } else {
            Ok(())
        }
    }

    pub(crate) fn is_there_greater_log(&self) -> bool {
        self.seen_greater_log
    }

    /// Set that there is greater last log id found.
    ///
    /// In such a case, this node should not try to elect aggressively.
    pub(crate) fn set_greater_log(&mut self) {
        self.seen_greater_log = true;
    }

    /// Clear the flag of that there is greater last log id.
    pub(crate) fn reset_greater_log(&mut self) {
        self.seen_greater_log = false;
    }

    // Only used by tests
    #[allow(dead_code)]
    pub(crate) fn calc_server_state(&self) -> ServerState {
        self.state.calc_server_state(&self.config.id)
    }

    // --- handlers ---

    pub(crate) fn vote_handler(&mut self) -> VoteHandler<C> {
        VoteHandler {
            config: &mut self.config,
            state: &mut self.state,
            output: &mut self.output,
            last_seen_vote: &mut self.last_seen_vote,
            leader: &mut self.leader,
            candidate: &mut self.candidate,
        }
    }

    pub(crate) fn log_handler(&mut self) -> LogHandler<C> {
        LogHandler {
            config: &mut self.config,
            state: &mut self.state,
            output: &mut self.output,
        }
    }

    pub(crate) fn snapshot_handler(&mut self) -> SnapshotHandler<C> {
        SnapshotHandler {
            state: &mut self.state,
            output: &mut self.output,
        }
    }

    pub(crate) fn leader_handler(&mut self) -> Result<LeaderHandler<C>, ForwardToLeader<C>> {
        let leader = match self.leader.as_mut() {
            None => {
                tracing::debug!("this node is NOT a leader: {:?}", self.state.server_state);
                return Err(self.state.forward_to_leader());
            }
            Some(x) => x,
        };

        // This leader is not accepted by a quorum yet.
        // Not a valid leader.
        //
        // Note that leading state is separated from local RaftState(which is used by the `Acceptor` part),
        // and do not consider the vote in the local RaftState.
        if !leader.vote.is_committed() {
            return Err(self.state.forward_to_leader());
        }

        Ok(LeaderHandler {
            config: &mut self.config,
            leader,
            state: &mut self.state,
            output: &mut self.output,
        })
    }

    pub(crate) fn replication_handler(&mut self) -> ReplicationHandler<C> {
        let leader = match self.leader.as_mut() {
            None => {
                unreachable!("There is no leader, can not handle replication");
            }
            Some(x) => x,
        };

        ReplicationHandler {
            config: &mut self.config,
            leader,
            state: &mut self.state,
            output: &mut self.output,
        }
    }

    pub(crate) fn following_handler(&mut self) -> FollowingHandler<C> {
        debug_assert!(self.leader.is_none());

        FollowingHandler {
            config: &mut self.config,
            state: &mut self.state,
            output: &mut self.output,
        }
    }

    pub(crate) fn server_state_handler(&mut self) -> ServerStateHandler<C> {
        ServerStateHandler {
            config: &self.config,
            state: &mut self.state,
            output: &mut self.output,
        }
    }
    pub(crate) fn establish_handler(&mut self) -> EstablishHandler<C> {
        EstablishHandler {
            config: &mut self.config,
            leader: &mut self.leader,
        }
    }
}

/// Supporting utilities for unit test
#[cfg(test)]
mod engine_testing {
    use crate::engine::Engine;
    use crate::proposer::LeaderQuorumSet;
    use crate::RaftTypeConfig;

    impl<C> Engine<C>
    where C: RaftTypeConfig
    {
        /// Create a Leader state just for testing purpose only,
        /// without initializing related resource,
        /// such as setting up replication, propose blank log.
        pub(crate) fn testing_new_leader(&mut self) -> &mut crate::proposer::Leader<C, LeaderQuorumSet<C::NodeId>> {
            let leader = self.state.new_leader();
            self.leader = Some(Box::new(leader));
            self.leader.as_mut().unwrap()
        }
    }
}
