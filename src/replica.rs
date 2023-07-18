use core::ops::RangeBounds;

use crate::*;

/// A CRDT for text.
///
/// Like all other text CRDTs it allows multiple peers on a distributed
/// network to concurrently edit the same text document, making sure that they
/// all converge to the same final state without relying on a central server to
/// coordinate the edits.
///
/// However, unlike many other CRDTs, a `Replica` doesn't actually store the
/// text contents itself. This allows to decouple the text buffer from the CRDT
/// machinery needed to guarantee convergence in the face of concurrency.
///
/// Put another way, a `Replica` is a pure CRDT that doesn't know anything
/// about where the text is actually stored. This is great because it makes it
/// very easy to use it in conjuction with any text data structure of your
/// choice: simple `String`s, gap buffers, piece tables, ropes, etc.
///
/// When starting a new collaborative editing session, the first peer
/// initializes its `Replica` via the [`new`](Self::new) method and sends it
/// to the other peers in the session.
///
/// Then, every time a peer performs an edit on their local buffer they inform
/// their `Replica` by calling either [`inserted`](Self::inserted) or
/// [`deleted`](Self::deleted). This produces a [`CrdtEdit`] which can be sent
/// over to the other peers using the network layer of your choice.
///
/// When a peer receives a `CrdtEdit` they can integrate it into their own
/// `Replica` by calling the [`merge`](Self::merge) method. This produces a
/// [`TextEdit`] which tells them *where* in their local buffer they should
/// apply the edit, taking into account all the other edits that have happened
/// concurrently.
///
/// Basically, you tell your `Replica` how your buffer changes, and it tells
/// you how your buffer *should* change when receiving remote edits.
pub struct Replica {
    /// TODO: docs
    id: ReplicaId,

    /// TODO: docs
    run_tree: RunTree,

    /// TODO: docs
    run_indices: RunIndices,

    /// TODO: docs
    lamport_clock: LamportClock,

    /// TODO: docs
    insertion_clock: InsertionClock,

    /// TODO: docs
    version_map: VersionMap,

    /// TODO: docs
    deletion_map: DeletionMap,

    /// TODO: docs
    backlog: BackLog,
}

impl Replica {
    #[doc(hidden)]
    pub fn assert_invariants(&self) {
        self.run_tree.assert_invariants();
        self.run_indices.assert_invariants(&self.run_tree);
    }

    #[doc(hidden)]
    pub fn average_gtree_inode_occupancy(&self) -> f32 {
        self.run_tree.average_inode_occupancy()
    }

    /// Sometimes the [`merge`](Replica::merge) method is not able to produce a
    /// `TextEdit` for the given `CrdtEdit` at the time it is called. This is
    /// usually because the `CrdtEdit` is itself dependent on some context that
    /// the `Replica` may not have yet.
    ///
    /// When this happens, the `Replica` stores the `CrdtEdit` in an internal
    /// backlog of edits that can't be processed yet, but may be in the future.
    ///
    /// This method returns an iterator over all the backlogged edits which are
    /// now ready to be applied to your buffer.
    ///
    /// The [`BackLogged`] iterator yields [`TextEdit`]s. It's very important
    /// that you apply every `TextEdit` to your buffer in the *exact same*
    /// order in which they were yielded by the iterator. If you don't your
    /// buffer could permanently diverge from the other peers.
    ///
    /// # Example
    /// ```
    /// # use cola::{Replica, TextEdit};
    /// // The buffer at peer 1 is "ab".
    /// let mut replica1 = Replica::new(1, 2);
    ///
    /// // A second peer joins the session.
    /// let mut replica2 = replica1.fork(2);
    ///
    /// // Peer 1 inserts 'c', 'd' and 'e' at the end of the buffer.
    /// let insert_c = replica1.inserted(2, 1);
    /// let insert_d = replica1.inserted(3, 1);
    /// let insert_e = replica1.inserted(4, 1);
    ///
    /// // For some reason, the network layer messes up the order of the edits
    /// // and they get to the second peer in the opposite order. Because each
    /// // edit depends on the previous one, peer 2 can't merge the insertions
    /// // of the 'd' and the 'e' until it sees the 'c'.
    /// let none_e = replica2.merge(&insert_e);
    /// let none_d = replica2.merge(&insert_d);
    ///
    /// assert!(none_e.is_none());
    /// assert!(none_d.is_none());
    ///
    /// // Finally, peer 2 receives the 'c' and it's able merge it right away.
    /// let Some(TextEdit::Insertion(offset_c, _)) = replica2.merge(&insert_c)
    /// else {
    ///     unreachable!()
    /// };
    ///
    /// assert_eq!(offset_c, 2);
    ///
    /// // Peer 2 now has all the context it needs to merge the rest of the
    /// // edits that were previously backlogged.
    /// let mut backlogged = replica2.backlogged();
    ///
    /// assert!(matches!(backlogged.next(), Some(TextEdit::Insertion(3, _))));
    /// assert!(matches!(backlogged.next(), Some(TextEdit::Insertion(4, _))));
    /// ```
    #[inline]
    pub fn backlogged(&mut self) -> BackLogged<'_> {
        BackLogged::from_replica(self)
    }

    /// TOOD: docs
    #[inline]
    pub(crate) fn backlog_mut(&mut self) -> &mut BackLog {
        &mut self.backlog
    }

    /// TODO: docs
    #[inline]
    pub(crate) fn can_merge_deletion(&self, deletion: &Deletion) -> bool {
        debug_assert!(!self.has_merged_deletion(deletion));

        (
            // TODO: docs
            self.deletion_map.get(deletion.deleted_by()) + 1
                == deletion.deletion_ts
        ) && (
            // TODO: docs
            self.version_map >= deletion.version_map
        )
    }

    /// TODO: docs
    #[inline]
    pub(crate) fn can_merge_insertion(&self, insertion: &Insertion) -> bool {
        debug_assert!(!self.has_merged_insertion(insertion));

        (
            // Makes sure that we merge insertions in the same order they were
            // created.
            //
            // This is technically not needed to merge a single insertion (all
            // that matters is that we know where to anchor the insertion), but
            // it's needed to correctly increment the chararacter clock inside
            // this `Replica`'s `VersionMap` without skipping any temporal
            // range.
            self.version_map.get(insertion.inserted_by()) == insertion.start()
        ) && (
            // Makes sure that we have already merged the insertion containing
            // the anchor of this insertion.
            self.version_map.get(insertion.anchor().replica_id())
                >= insertion.anchor().character_ts()
        )
    }

    #[doc(hidden)]
    pub fn debug(&self) -> debug::DebugAsSelf<'_> {
        self.into()
    }

    #[doc(hidden)]
    pub fn debug_as_btree(&self) -> debug::DebugAsBtree<'_> {
        self.into()
    }

    /// Creates a new `Replica` with the given id by decoding the contents of
    /// the [`EncodedReplica`].
    ///
    /// # Example
    ///
    /// ```
    /// # use cola::{Replica, EncodedReplica};
    /// let replica1 = Replica::new(1, 42);
    ///
    /// let encoded: EncodedReplica = replica1.encode();
    ///
    /// let replica2 = Replica::decode(2, &encoded).unwrap();
    ///
    /// assert_eq!(replica2.id(), ReplicaId::from(2));
    /// ```
    #[cfg(feature = "encode")]
    #[cfg_attr(docsrs, doc(cfg(feature = "encode")))]
    #[inline]
    pub fn decode<Id>(
        id: Id,
        encoded: &EncodedReplica,
    ) -> Result<Self, DecodeError>
    where
        Id: Into<ReplicaId>,
    {
        if encoded.protocol_version() != PROTOCOL_VERSION {
            return Err(DecodeError::DifferentProtocol {
                encoded_on: encoded.protocol_version(),
                decoding_on: PROTOCOL_VERSION,
            });
        }

        if encoded.checksum() != &checksum(encoded.bytes()) {
            return Err(DecodeError::ChecksumFailed);
        }

        let Some((
            run_tree,
            run_indices,
            lamport_clock,
            mut version_map,
            mut deletion_map,
            backlog,
        )) = encode::decode(encoded.bytes())
        else {
            return Err(DecodeError::InvalidData);
        };

        let id = id.into();

        version_map.fork_in_place(id, 0);
        deletion_map.fork_in_place(id, 1);

        let replica = Self {
            id,
            run_tree,
            run_indices,
            insertion_clock: InsertionClock::new(),
            lamport_clock: lamport_clock.fork(),
            version_map,
            deletion_map,
            backlog,
        };

        Ok(replica)
    }

    /// Informs the `Replica` that you have deleted the characters in the given
    /// offset range.
    ///
    /// This produces a [`CrdtEdit`] which can be sent to all the other peers
    /// to integrate the deletion into their own `Replica`s.
    ///
    /// # Panics
    ///
    /// Panics if the start of the range is greater than the end or if the end
    /// is out of bounds (i.e. greater than the current length of your buffer).
    ///
    /// # Example
    ///
    /// ```
    /// # use cola::{Replica, TextEdit};
    /// // The buffer at peer 1 is "Hello World".
    /// let mut replica1 = Replica::new(1, 11);
    ///
    /// // Peer 1 deletes "Hello ".
    /// let edit: CrdtEdit = replica1.deleted(..6);
    /// ```
    #[inline]
    pub fn deleted<R>(&mut self, range: R) -> CrdtEdit
    where
        R: RangeBounds<Length>,
    {
        let (start, end) = range_bounds_to_start_end(range, 0, self.len());

        if start == end {
            return CrdtEdit::no_op();
        }

        let deleted_range = (start..end).into();

        let (start, end, outcome) = self.run_tree.delete(deleted_range);

        match outcome {
            DeletionOutcome::DeletedAcrossRuns { split_start, split_end } => {
                if let Some((replica_id, insertion_ts, offset, idx)) =
                    split_start
                {
                    self.run_indices.get_mut(replica_id).split(
                        insertion_ts,
                        offset,
                        idx,
                    );
                }
                if let Some((replica_id, insertion_ts, offset, idx)) =
                    split_end
                {
                    self.run_indices.get_mut(replica_id).split(
                        insertion_ts,
                        offset,
                        idx,
                    );
                }
            },

            DeletionOutcome::DeletedInMiddleOfSingleRun {
                replica_id,
                insertion_ts,
                range,
                idx_of_deleted,
                idx_of_split,
            } => {
                let indices = self.run_indices.get_mut(replica_id);
                indices.split(insertion_ts, range.start, idx_of_deleted);
                indices.split(insertion_ts, range.end, idx_of_split);
            },

            DeletionOutcome::DeletionSplitSingleRun {
                replica_id,
                insertion_ts,
                offset,
                idx,
            } => self.run_indices.get_mut(replica_id).split(
                insertion_ts,
                offset,
                idx,
            ),

            DeletionOutcome::DeletionMergedInPreviousRun {
                replica_id,
                insertion_ts,
                offset,
                deleted,
            } => {
                self.run_indices.get_mut(replica_id).move_len_to_prev_split(
                    insertion_ts,
                    offset,
                    deleted,
                );
            },

            DeletionOutcome::DeletionMergedInNextRun {
                replica_id,
                insertion_ts,
                offset,
                deleted,
            } => {
                self.run_indices.get_mut(replica_id).move_len_to_next_split(
                    insertion_ts,
                    offset,
                    deleted,
                );
            },

            DeletionOutcome::DeletedWholeRun => {},
        }

        let deletion_ts = self.deletion_map.this();

        *self.deletion_map.this_mut() += 1;

        CrdtEdit::deletion(start, end, self.version_map.clone(), deletion_ts)
    }

    #[doc(hidden)]
    pub fn empty_leaves(&self) -> (usize, usize) {
        self.run_tree.count_empty_leaves()
    }

    #[doc(hidden)]
    pub fn eq_decoded(&self, other: &Self) -> bool {
        self.run_tree == other.run_tree
            && self.run_indices == other.run_indices
            && self.backlog == other.backlog
    }

    /// Encodes the `Replica` in a custom binary format.
    ///
    /// This can be used to send a `Replica` to another peer over the network.
    /// Once they have received the [`EncodedReplica`] they can decode it via
    /// the [`decode`](Replica::decode) method.
    ///
    /// Note that if you want to collaborate within a single process you can
    /// just [`fork`](Replica::fork) the `Replica` without having to encode it
    /// and decode it again.
    #[cfg(feature = "encode")]
    #[cfg_attr(docsrs, doc(cfg(feature = "encode")))]
    #[inline]
    pub fn encode(&self) -> EncodedReplica {
        let bytes = encode::encode(self);
        let checksum = checksum(&bytes);
        EncodedReplica::new(PROTOCOL_VERSION, checksum, bytes)
    }

    /// Creates a new `Replica` with the given id but with the same internal
    /// state as this one.
    ///
    /// Note that this method should be used when the collaborative session is
    /// limited to a single process (e.g. multiple threads working on the same
    /// document). If you want to collaborate across different processes or
    /// machines you should [`encode`](Replica::encode) the `Replica` and send
    /// the result to the other peers.
    ///
    /// # Example
    ///
    /// ```
    /// # use cola::Replica;
    /// let replica1 = Replica::new(1, 0);
    /// let replica2 = replica1.fork(2);
    /// assert_eq!(replica2.id(), ReplicaId::from(2))
    /// ```
    #[inline]
    pub fn fork<Id>(&self, new_id: Id) -> Self
    where
        Id: Into<ReplicaId>,
    {
        let new_id = new_id.into();

        Self {
            id: new_id,
            run_tree: self.run_tree.clone(),
            run_indices: self.run_indices.clone(),
            insertion_clock: InsertionClock::new(),
            lamport_clock: self.lamport_clock.fork(),
            version_map: self.version_map.fork(new_id, 0),
            deletion_map: self.deletion_map.fork(new_id, 1),
            backlog: self.backlog.clone(),
        }
    }

    /// TODO: docs
    #[inline]
    fn handle_insertion_outcome(
        &mut self,
        len: Length,
        outcome: InsertionOutcome,
    ) {
        match outcome {
            InsertionOutcome::ExtendedLastRun { replica_id } => {
                self.run_indices.get_mut(replica_id).extend_last(len)
            },

            InsertionOutcome::SplitRun {
                split_id,
                split_insertion,
                split_at_offset,
                split_idx,
                inserted_id,
                inserted_idx,
            } => {
                self.run_indices
                    .get_mut(inserted_id)
                    .append(len, inserted_idx);

                self.run_indices.get_mut(split_id).split(
                    split_insertion,
                    split_at_offset,
                    split_idx,
                );
            },

            InsertionOutcome::InsertedRun { replica_id, inserted_idx } => {
                self.run_indices.get_mut(replica_id).append(len, inserted_idx)
            },
        };
    }

    /// TODO: docs
    #[inline]
    fn has_merged_deletion(&self, deletion: &Deletion) -> bool {
        self.deletion_map.get(deletion.deleted_by()) > deletion.deletion_ts
    }

    /// TODO: docs
    #[inline]
    fn has_merged_insertion(&self, insertion: &Insertion) -> bool {
        self.version_map.get(insertion.inserted_by()) > insertion.start()
    }

    /// Returns the id of the `Replica`.
    #[inline]
    pub fn id(&self) -> ReplicaId {
        self.id
    }

    /// Informs the `Replica` that you have inserted `len` characters at the
    /// given offset.
    ///
    /// This produces a [`CrdtEdit`] which can be sent to all the other peers
    /// to integrate the insertion into their own `Replica`s.
    ///
    /// # Panics
    ///
    /// Panics if the offset is out of bounds (i.e. greater than the current
    /// length of your buffer).
    ///
    /// # Example
    ///
    /// ```
    /// # use cola::{Replica, TextEdit};
    /// // The buffer at peer 1 is "ab".
    /// let mut replica1 = Replica::new(1, 2);
    ///
    /// // Peer 1 inserts two characters between the 'a' and the 'b'.
    /// let edit: CrdtEdit = replica1.inserted(1, 2);
    /// ```
    #[inline]
    pub fn inserted(&mut self, at_offset: Length, len: Length) -> CrdtEdit {
        if len == 0 {
            return CrdtEdit::no_op();
        }

        let start = self.version_map.this();

        *self.version_map.this_mut() += len;

        let end = self.version_map.this();

        let text = Text::new(self.id, start..end);

        let (anchor, anchor_ts, outcome) = self.run_tree.insert(
            at_offset,
            text.clone(),
            &mut self.insertion_clock,
            &mut self.lamport_clock,
        );

        self.handle_insertion_outcome(len, outcome);

        CrdtEdit::insertion(
            anchor,
            anchor_ts,
            text,
            self.lamport_clock.last(),
            self.insertion_clock.last(),
        )
    }

    #[allow(clippy::len_without_is_empty)]
    #[doc(hidden)]
    pub fn len(&self) -> Length {
        self.run_tree.len()
    }

    /// Merges a [`CrdtEdit`] created by another peer into this `Replica`,
    /// optionally producing a [`TextEdit`] which can be applied to your
    /// buffer.
    ///
    /// There can be multiple reasons why this method returns `None`, for
    /// example:
    ///
    /// - the `CrdtEdit` is a no-op, like inserting zero characters or deleting
    /// an empty range;
    ///
    /// - the `CrdtEdit` was created by the same `Replica` that's now trying to
    /// merge it (i.e. you're trying to merge your own edits);
    ///
    /// - the same `CrdtEdit` has already been merged by this `Replica`
    /// (merging the same edit multiple times is idempotent);
    ///
    /// - the `CrdtEdit` depends on some context that the `Replica` doesn't yet
    /// have (see the [`backlogged`](Replica::backlogged) method which handles
    /// this case);
    ///
    /// - etc.
    ///
    /// If you do get a `Some` value, it's very important to apply the returned
    /// `TextEdit` to your buffer *before* processing any other edits (both
    /// remote and local). This is because `TextEdit`s refer to the state of
    /// the buffer at the time they were created. If the state changes before
    /// you apply a `TextEdit`, its coordinates might no longer be valid.
    ///
    /// # Example
    ///
    /// ```
    /// # use cola::{Replica, TextEdit};
    /// // Peer 1 starts with a buffer containing "abcd" and sends it over to a
    /// // second peer.
    /// let mut replica1 = Replica::new(1, 4);
    /// let mut replica2 = replica1.fork(2);
    ///
    /// // Peer 1 inserts a character between the 'b' and the 'c'.
    /// let insertion_at_1 = replica1.inserted(2, 1);
    ///
    /// // Concurrently with the insertion, peer 2 deletes the 'b'.
    /// let deletion_at_2 = replica2.deleted(1..2);
    ///
    /// // The two peers exchange their edits.
    ///
    /// // The deletion arrives at the first peer. There have not been any
    /// // insertions or deletions *before* the 'b', so its offset range should
    /// // still be 1..2.
    /// let Some(TextEdit::ContiguousDeletion(range_b)) = replica1.merge(&deletion_at_2) else {
    ///     unreachable!();
    /// }
    ///
    /// assert_eq!(range_b, 1..2);
    ///
    /// // Finally, the insertion arrives at the second peer. Here the 'b' has
    /// // been deleted, so the offset at which we should insert the new
    /// // character is not 2, but 1. This is because the *intent* of the first
    /// // peer was to insert the character between the 'b' and the 'c'.
    /// let Some(TextEdit::Insertion(offset, _)) = replica2.merge(&insertion_at_1) else {
    ///     unreachable!();
    /// };
    ///
    /// assert_eq!(offset, 1);
    /// ```
    #[inline]
    pub fn merge(&mut self, crdt_edit: &CrdtEdit) -> Option<TextEdit> {
        match crdt_edit.kind() {
            CrdtEditKind::Insertion(insertion)
                if !self.has_merged_insertion(insertion) =>
            {
                self.merge_insertion(insertion)
            },

            CrdtEditKind::Deletion(deletion)
                if !self.has_merged_deletion(deletion) =>
            {
                self.merge_deletion(deletion)
            },

            _ => None,
        }
    }

    /// TODO: docs
    #[inline]
    fn merge_deletion(&mut self, deletion: &Deletion) -> Option<TextEdit> {
        debug_assert!(!self.has_merged_deletion(deletion));

        if self.can_merge_deletion(deletion) {
            self.merge_unchecked_deletion(deletion)
        } else {
            self.backlog.add_deletion(deletion.clone());
            None
        }
    }

    /// TODO: docs
    #[inline]
    fn merge_insertion(&mut self, insertion: &Insertion) -> Option<TextEdit> {
        debug_assert!(!self.has_merged_insertion(insertion));

        if self.can_merge_insertion(insertion) {
            Some(self.merge_unchecked_insertion(insertion))
        } else {
            self.backlog.add_insertion(insertion.clone());
            None
        }
    }

    /// TODO: docs
    #[inline]
    pub(crate) fn merge_unchecked_deletion(
        &mut self,
        deletion: &Deletion,
    ) -> Option<TextEdit> {
        debug_assert!(self.can_merge_deletion(deletion));

        let outcome = self.run_tree.merge_deletion(deletion);

        *self.deletion_map.get_mut(deletion.deleted_by()) =
            deletion.deletion_ts;

        match outcome {
            MergedDeletion::Contiguous(range) => {
                Some(TextEdit::ContiguousDeletion(range))
            },

            MergedDeletion::Split(ranges) => {
                Some(TextEdit::SplitDeletion(ranges))
            },
        }
    }

    /// TODO: docs
    #[inline]
    pub(crate) fn merge_unchecked_insertion(
        &mut self,
        insertion: &Insertion,
    ) -> TextEdit {
        debug_assert!(self.can_merge_insertion(insertion));

        let (offset, outcome) =
            self.run_tree.merge_insertion(insertion, &self.run_indices);

        let len = insertion.len();

        self.handle_insertion_outcome(len, outcome);

        *self.version_map.get_mut(insertion.inserted_by()) += len;

        self.lamport_clock.update(insertion.lamport_ts());

        TextEdit::Insertion(offset, insertion.text().clone())
    }

    /// Creates a new `Replica` with the given id from the initial [`Length`]
    /// of your buffer.
    ///
    /// Note that if you have multiple peers working on the same document you
    /// should only use this constructor on the first peer, usually the one
    /// that starts the collaboration session.
    ///
    /// The other peers should get their `Replica` from another `Replica`
    /// already in the session by either:
    ///
    /// a) [`fork`](Replica::fork)ing it if the collaboration happens all in
    /// the same process (e.g. a text editor with plugins running on separate
    /// threads),
    ///
    /// b) [`encode`](Replica::encode)ing it and sending the result over the
    /// network if the collaboration is between different processes or
    /// machines.
    ///
    /// # Example
    /// ```
    /// # use std::thread;
    /// # use cola::Replica;
    /// // A text editor initializes a new Replica on the main thread where the
    /// // buffer is "foo".
    /// let replica_main = Replica::new(0, 3);
    ///
    /// // It then starts a plugin on a separate thread and wants to give it a
    /// // Replica to keep its buffer synchronized with the one on the main
    /// // thread. It does *not* call `new()` again, but instead clones the
    /// // existing Replica and sends it to the new thread.
    /// let replica_plugin = replica_main.fork(1);
    ///
    /// thread::spawn(move || {
    ///     // The plugin can now use its Replica to exchange edits with the
    ///     // main thread.
    ///     println!("{replica_plugin:?}");
    /// });
    /// ```
    #[inline]
    pub fn new<Id>(id: Id, len: Length) -> Self
    where
        Id: Into<ReplicaId>,
    {
        let id = id.into();

        let mut insertion_clock = InsertionClock::new();

        let mut lamport_clock = LamportClock::new();

        let initial_text = Text::new(id, 0..len);

        let origin_run = EditRun::new(
            Anchor::origin(),
            initial_text,
            insertion_clock.next(),
            lamport_clock.next(),
        );

        let (run_tree, origin_idx) = RunTree::new(origin_run);

        let run_indices = RunIndices::new(id, origin_idx, len);

        Self {
            id,
            run_tree,
            run_indices,
            insertion_clock,
            lamport_clock,
            version_map: VersionMap::new(id, len),
            deletion_map: DeletionMap::new(id, 1),
            backlog: BackLog::new(),
        }
    }
}

impl core::fmt::Debug for Replica {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        struct DebugHexU64(u64);

        impl core::fmt::Debug for DebugHexU64 {
            fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
                write!(f, "{:x}", self.0)
            }
        }

        // In the public Debug we just print the ReplicaId to avoid leaking
        // our internals.
        //
        // During development the `Replica::debug()` method (which is public
        // but hidden from the API) can be used to obtain a more useful
        // representation.
        f.debug_tuple("Replica").field(&DebugHexU64(self.id.as_u64())).finish()
    }
}

/// TODO: docs
#[derive(Copy, Clone, Default)]
#[cfg_attr(feature = "encode", derive(serde::Serialize, serde::Deserialize))]
pub struct LamportClock(u64);

impl core::fmt::Debug for LamportClock {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        write!(f, "LamportClock({})", self.0)
    }
}

impl LamportClock {
    #[inline]
    fn fork(&self) -> Self {
        Self(self.0 + 1)
    }

    #[inline]
    fn last(&self) -> LamportTimestamp {
        self.0.saturating_sub(1)
    }

    #[inline]
    fn new() -> Self {
        Self::default()
    }

    /// TODO: docs
    #[inline]
    pub fn next(&mut self) -> LamportTimestamp {
        let next = self.0;
        self.0 += 1;
        next
    }

    /// TODO: docs
    #[inline]
    fn update(&mut self, other: LamportTimestamp) {
        self.0 = self.0.max(other) + 1;
    }
}

/// TODO: docs
pub type LamportTimestamp = u64;

/// TODO: docs
#[derive(Copy, Clone, Default)]
pub struct InsertionClock(u64);

impl core::fmt::Debug for InsertionClock {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        write!(f, "InsertionClock({})", self.0)
    }
}

impl InsertionClock {
    #[inline]
    fn last(&self) -> LamportTimestamp {
        self.0.saturating_sub(1)
    }

    #[inline]
    fn new() -> Self {
        Self::default()
    }

    /// TODO: docs
    #[inline]
    pub fn next(&mut self) -> InsertionTimestamp {
        let next = self.0;
        self.0 += 1;
        next
    }
}

/// TODO: docs
pub type InsertionTimestamp = u64;

/// TODO: docs
pub type DeletionClock = u64;

/// TODO: docs
pub type DeletionTs = DeletionClock;

#[cfg(feature = "encode")]
mod encode {
    use serde::{de, ser};

    use super::*;

    type EncodedFields =
        (RunTree, RunIndices, LamportClock, VersionMap, DeletionMap, BackLog);

    /// TODO: docs
    #[inline]
    pub(super) fn encode(replica: &Replica) -> Vec<u8> {
        let mut encoded = Vec::new();

        encode_field(&mut encoded, &replica.run_tree);
        encode_field(&mut encoded, &replica.run_indices);
        encode_field(&mut encoded, &replica.lamport_clock);
        encode_field(&mut encoded, &replica.version_map);
        encode_field(&mut encoded, &replica.deletion_map);
        encode_field(&mut encoded, &replica.backlog);

        encoded
    }

    /// TODO: docs
    #[inline]
    pub(super) fn decode(bytes: &[u8]) -> Option<EncodedFields> {
        let (run_tree, bytes) = decode_field(bytes)?;
        let (run_indices, bytes) = decode_field(bytes)?;
        let (lamport_clock, bytes) = decode_field(bytes)?;
        let (version_map, bytes) = decode_field(bytes)?;
        let (deletion_map, bytes) = decode_field(bytes)?;
        let (backlog, bytes) = decode_field(bytes)?;

        if bytes.is_empty() {
            Some((
                run_tree,
                run_indices,
                lamport_clock,
                version_map,
                deletion_map,
                backlog,
            ))
        } else {
            None
        }
    }

    #[inline]
    fn encode_field<T>(buf: &mut Vec<u8>, field: &T)
    where
        T: ser::Serialize,
    {
        let field_bytes = serialize(field);
        let len_bytes = field_bytes.len().to_le_bytes();
        buf.extend_from_slice(&len_bytes);
        buf.extend_from_slice(&field_bytes);
    }

    #[inline]
    fn decode_field<'a, T>(buf: &'a [u8]) -> Option<(T, &'a [u8])>
    where
        T: de::Deserialize<'a>,
    {
        // The first 8 bytes represent the length of the encoded field.
        let (len_bytes, rest) = if buf.len() >= 8 {
            buf.split_at(8)
        } else {
            return None;
        };

        let len_bytes: [u8; 8] = len_bytes.try_into().ok()?;

        let len = usize::from_le_bytes(len_bytes);

        let (encoded_field, rest) = if rest.len() >= len {
            rest.split_at(len)
        } else {
            return None;
        };

        deserialize::<T>(encoded_field).map(|field| (field, rest))
    }

    #[inline]
    fn serialize<T>(value: &T) -> Vec<u8>
    where
        T: ser::Serialize,
    {
        bincode::serialize(value).expect("failed to serialize")
    }

    #[inline]
    fn deserialize<'a, T>(bytes: &'a [u8]) -> Option<T>
    where
        T: de::Deserialize<'a>,
    {
        bincode::deserialize(bytes).ok()
    }
}

mod debug {
    use core::fmt::Debug;

    use super::*;

    pub struct DebugAsSelf<'a>(BaseDebug<'a, run_tree::DebugAsSelf<'a>>);

    impl<'a> From<&'a Replica> for DebugAsSelf<'a> {
        #[inline]
        fn from(replica: &'a Replica) -> DebugAsSelf<'a> {
            let base = BaseDebug {
                replica,
                debug_run_tree: replica.run_tree.debug_as_self(),
            };

            Self(base)
        }
    }

    impl<'a> core::fmt::Debug for DebugAsSelf<'a> {
        fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
            self.0.fmt(f)
        }
    }

    pub struct DebugAsBtree<'a>(BaseDebug<'a, run_tree::DebugAsBtree<'a>>);

    impl<'a> From<&'a Replica> for DebugAsBtree<'a> {
        #[inline]
        fn from(replica: &'a Replica) -> DebugAsBtree<'a> {
            let base = BaseDebug {
                replica,
                debug_run_tree: replica.run_tree.debug_as_btree(),
            };

            Self(base)
        }
    }

    impl<'a> core::fmt::Debug for DebugAsBtree<'a> {
        fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
            self.0.fmt(f)
        }
    }

    struct BaseDebug<'a, T: Debug> {
        replica: &'a Replica,
        debug_run_tree: T,
    }

    impl<'a, T: Debug> Debug for BaseDebug<'a, T> {
        fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
            let replica = &self.replica;

            f.debug_struct("Replica")
                .field("id", &replica.id)
                .field("run_tree", &self.debug_run_tree)
                .field("run_indices", &replica.run_indices)
                .field("lamport_clock", &replica.lamport_clock)
                .field("insertion_clock", &replica.insertion_clock)
                .field("version_map", &replica.version_map)
                .field("deletion_map", &replica.deletion_map)
                .field("backlog", &replica.backlog)
                .finish()
        }
    }
}
