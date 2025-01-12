use std::{cell::UnsafeCell, mem::MaybeUninit, ops::{Deref, DerefMut}, ptr::NonNull, sync::{atomic::{AtomicUsize, Ordering}, Mutex, PoisonError}, task::{Context, Poll, Waker}};

use states::{Owner, RefState};

struct Contract<T> {
    /// The number of refereces pointing to the contract (includes weak references and owner).
    ref_count: AtomicUsize,
    /// Stores the shared data.
    value: UnsafeCell<MaybeUninit<T>>,
    /// Synchronization information.
    contract_state: Mutex<ContractState>,
}

pub struct ContractState {
    /// Contains important counts
    counts: ContractCounts,
    /// Contains queued borrows
    wakers: BorrowQueue,
}

impl ContractState {
    pub fn new() -> Self {
        ContractState { counts: ContractCounts::new(), wakers: BorrowQueue::new() }
    }

    pub fn reduce_borrow(&mut self, from: &BorrowKind) -> AdditionalWork {
        if self.counts.borrow_count == 0 {
            self.wake_from(BorrowKind::Empty)
        } else if from.is_exotic() && self.counts.exotic_count == 0 {
            self.wake_from(BorrowKind::Ref)
        } else {
            AdditionalWork::Noop
        }
    }

    /// Casts the borrow and handles waking
    pub fn cast_borrow(&mut self, from: &BorrowKind, to: &BorrowKind) {
        self.counts.record_borrow_release(from);
        self.counts.record_borrow_acquire(to);
        if to < from {
            // not the last time a wake will happen, no additional cleanup required.
            let _ = self.reduce_borrow(from);
        }
    }

    /// Casts the borrow without handling waking
    pub fn silent_cast_borrow(&mut self, from: &BorrowKind, to: &BorrowKind) {
        self.counts.record_borrow_release(from);
        self.counts.record_borrow_acquire(to);
    }

    pub fn wake_from(&mut self, kind: BorrowKind) -> AdditionalWork {
        self.wakers.wake_from(kind, &mut self.counts)
    }
}

pub struct ContractCounts {
    /// The number of exotic references that could upgrade to become mutable (or are already mutable).
    exotic_count: usize,
    /// The number of strong references held.
    borrow_count: usize,
    /// Number of mutable borrows thusfar (counts upgrading, owner re-aquisition and dropping).
    mut_number: u64,
}

impl ContractCounts {
    pub fn new() -> Self {
        ContractCounts { exotic_count: 0, borrow_count: 0, mut_number: 0 }
    }

    pub fn record_borrow_acquire(&mut self, kind: &BorrowKind) {
        match kind {
            BorrowKind::Mut => {
                self.exotic_count += 1;
                self.borrow_count += 1;
                self.mut_number += 1;
            },
            BorrowKind::Upgradable => {
                self.exotic_count += 1;
                self.borrow_count += 1;
            }
            BorrowKind::Ref => {
                self.exotic_count += 1
            },
            BorrowKind::Empty => (),
        }
    }

    pub fn record_borrow_release(&mut self, kind: &BorrowKind) {
        match kind {
            BorrowKind::Mut | BorrowKind::Upgradable => {
                self.exotic_count -= 1;
                self.borrow_count -= 1;
            },
            BorrowKind::Ref => {
                self.exotic_count -= 1
            },
            BorrowKind::Empty => (),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdditionalWork {
    Noop,
    Drop,
}

mod borrow_queue {
    use std::{collections::VecDeque, future::Future, sync::Arc};

    use super::*;

    /// The status of a borrow forward node
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum NodeStatus {
        /// forwarding is ongoing, an empty queue will stop waking
        /// 
        /// this is the initial state
        Growing,
        /// forwarding has stopped, queue can only be drained
        /// 
        /// draining only when whatever process that was forwarding has stopped
        Shrinking,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum WakeContinuation {
        /// Waking interrupted
        Await,
        /// Waking complete
        Complete,
        /// Move on to next forward.
        Next
    }

    #[derive(Debug)]
    pub enum LastInteraction {
        NonBlocking,
        Blocked(BorrowKind),
        Queue,
    }

    impl LastInteraction {
        fn reset_state(&self) -> Self {
            match self {
                LastInteraction::NonBlocking | LastInteraction::Blocked(..) => LastInteraction::NonBlocking,
                LastInteraction::Queue => LastInteraction::Queue,
            }
        }

        pub fn generate_followup(&mut self, kind: &BorrowKind) -> QueueActionFollowup {
            match self {
                LastInteraction::NonBlocking => QueueActionFollowup::Done,
                LastInteraction::Blocked(borrow_kind) => if borrow_kind.would_wake(kind) {
                    std::mem::replace(self, self.reset_state());
                    QueueActionFollowup::Prompt
                } else {
                    QueueActionFollowup::Done
                },
                LastInteraction::Queue => if kind.is_exotic() {
                    QueueActionFollowup::Done
                } else {
                    QueueActionFollowup::Prompt
                },
            }
        }
    }

    pub struct BorrowQueueNode {
        last_interaction: LastInteraction,
        status: NodeStatus,
        // u64 to ensure it can count is high enough
        popped: u64,
        queue: VecDeque<BorrowWaker>,
        next: Option<Arc<Mutex<BorrowQueueNode>>>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum QueueActionFollowup {
        // ensure that you release the mutex for this node before attempting a drain or even locking the contract.
        Prompt,
        Done,
    }

    impl Default for QueueActionFollowup {
        fn default() -> Self {
            QueueActionFollowup::Done
        }
    }

    impl BorrowQueueNode {
        pub fn new_forward(next: Option<Arc<Mutex<BorrowQueueNode>>>) -> BorrowQueueNode {
            BorrowQueueNode {
                // will Blocking(BorrowKind::Empty) after dropping the mut
                last_interaction: LastInteraction::NonBlocking,
                status: NodeStatus::Growing,
                popped: 0,
                queue: VecDeque::new(),
                next
            }
        }

        pub fn new_upgrade_queue(next: Option<Arc<Mutex<BorrowQueueNode>>>) -> BorrowQueueNode {
            BorrowQueueNode {
                last_interaction: LastInteraction::Queue,
                status: NodeStatus::Growing,
                popped: 0,
                queue: VecDeque::new(),
                next
            }
        }

        pub fn get_mut(&mut self, id: &BorrowId) -> Option<&mut BorrowWaker> {
            let n = id.index.checked_sub(self.popped)?;
            self.queue.get_mut(n as usize)
        }

        pub fn poll_at(&mut self, id: &BorrowId, cx: &mut Context<'_>) -> Poll<()> {
            match self.get_mut(id) {
                Some(borrow_waker) => {
                    borrow_waker.set_waker(cx);
                    Poll::Pending
                },
                None => Poll::Ready(()),
            }
        }

        pub fn close_waker(&mut self, id: &BorrowId) -> Option<()> {
            let borrow_waker = self.get_mut(id)?;
            borrow_waker.kind = BorrowKind::Empty;
            Some(())
        }

        /// If result is not used a deadlock could occur
        #[must_use]
        pub fn enqueue(&mut self, this: Arc<Mutex<Self>>, kind: BorrowKind) -> (BorrowFuture, QueueActionFollowup) {
            debug_assert_eq!(self.status, NodeStatus::Growing);
            let id = BorrowId { index: (self.queue.len() as u64) + self.popped };
            let followup = self.queue
                .is_empty()
                .then(|| self.last_interaction.generate_followup(&kind))
                .unwrap_or_default();
            self.queue.push_back(BorrowWaker { kind, waker: futures::task::noop_waker() });
            (BorrowFuture { id, node: this }, followup )
        }

        /// If `None` then alread awoken.
        /// 
        /// If result is not used a deadlock could occur
        #[must_use]
        pub fn try_change(&mut self, id: &BorrowId, kind: BorrowKind) -> Option<QueueActionFollowup> {
            let borrow_waker = self.get_mut(id)?;
            let followup = (
                    kind < std::mem::replace(&mut borrow_waker.kind, kind)
                    && id.index == self.popped
                )
                .then(|| self.last_interaction.generate_followup(&kind))
                .unwrap_or_default();
            Some(followup)
        }

        /// Wake compatible borrows.
        /// 
        /// If `final_wake` is initially `Ref` then can only wake refs potentially ending in a non-mut exotic.
        /// If `final_wake` is initially `Empty` then anything may be waked as this may only happen when empty.
        #[must_use]
        pub fn waking_drain(&mut self, final_wake: &mut BorrowKind, counts: &mut ContractCounts) -> WakeContinuation {
            while let Some(borrow_waker) = self.queue.front() {
                match (*final_wake, borrow_waker.kind) {
                    (_, BorrowKind::Empty) => {
                        unsafe { self.queue.pop_front().unwrap_unchecked() }.wake();
                        self.popped += 1;
                        // don't record
                        continue;
                    }
                    (blocked@(BorrowKind::Upgradable | BorrowKind::Mut), _) => {
                        self.last_interaction = LastInteraction::Blocked(blocked);
                        return WakeContinuation::Complete;
                    }
                    (BorrowKind::Empty, kind@BorrowKind::Mut) => {
                        unsafe { self.queue.pop_front().unwrap_unchecked() }.wake();
                        self.popped += 1;
                        counts.record_borrow_acquire(&kind);
                        *final_wake = kind;
                        continue;
                    }
                    (blocked, BorrowKind::Mut) => {
                        self.last_interaction = LastInteraction::Blocked(blocked);
                        return WakeContinuation::Complete;
                    }
                    (BorrowKind::Empty | BorrowKind::Ref, kind@BorrowKind::Upgradable) => {
                        unsafe { self.queue.pop_front().unwrap_unchecked() }.wake();
                        self.popped += 1;
                        counts.record_borrow_acquire(&kind);
                        *final_wake = kind;
                        continue;
                    }
                    (_, kind@BorrowKind::Ref) => {
                        unsafe { self.queue.pop_front().unwrap_unchecked() }.wake();
                        self.popped += 1;
                        counts.record_borrow_acquire(&kind);
                        *final_wake = kind;
                        continue;
                    }
                }
                #[allow(unreachable_code)]
                {
                    unreachable!()
                }
            }
            // can only get here if popped all elements
            match self.status {
                NodeStatus::Growing => {    
                    self.last_interaction = LastInteraction::Blocked(*final_wake);
                    WakeContinuation::Await
                },
                NodeStatus::Shrinking => WakeContinuation::Next,
            }
        }

        /// Wake `Empty`'s and `Ref`'s from the start of an upgrade buffer.
        pub fn reduce_upgrade_buffer(&mut self, counts: &mut ContractCounts) {
            while let Some(borrow_waker) = self.queue.front() {
                match borrow_waker.kind {
                    BorrowKind::Mut => return,
                    BorrowKind::Upgradable => return,
                    kind@BorrowKind::Ref => {
                        unsafe { self.queue.pop_front().unwrap_unchecked() }.wake();
                        self.popped += 1;
                        counts.record_borrow_acquire(&kind);
                        continue;
                    },
                    BorrowKind::Empty => continue,
                }
                #[allow(unreachable_code)]
                {
                    unreachable!()
                }
            }
        }
    }

    pub struct BorrowId {
        // u64 to ensure it can count is high enough
        index: u64,
    }

    /// Ordering is given by privillages.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum BorrowKind {
        Mut,
        Upgradable,
        Ref,
        Empty,
    }

    impl PartialOrd for BorrowKind {
        fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
            use std::cmp::Ordering;
            Some(match (self, other) {
                (BorrowKind::Mut, BorrowKind::Mut) => Ordering::Equal,
                (BorrowKind::Mut, BorrowKind::Upgradable) => Ordering::Greater,
                (BorrowKind::Mut, BorrowKind::Ref) => Ordering::Greater,
                (BorrowKind::Mut, BorrowKind::Empty) => Ordering::Greater,
                (BorrowKind::Upgradable, BorrowKind::Mut) => Ordering::Less,
                (BorrowKind::Upgradable, BorrowKind::Upgradable) => Ordering::Equal,
                (BorrowKind::Upgradable, BorrowKind::Ref) => Ordering::Greater,
                (BorrowKind::Upgradable, BorrowKind::Empty) => Ordering::Greater,
                (BorrowKind::Ref, BorrowKind::Mut) => Ordering::Less,
                (BorrowKind::Ref, BorrowKind::Upgradable) => Ordering::Less,
                (BorrowKind::Ref, BorrowKind::Ref) => Ordering::Equal,
                (BorrowKind::Ref, BorrowKind::Empty) => Ordering::Greater,
                (BorrowKind::Empty, BorrowKind::Mut) => Ordering::Less,
                (BorrowKind::Empty, BorrowKind::Upgradable) => Ordering::Less,
                (BorrowKind::Empty, BorrowKind::Ref) => Ordering::Less,
                (BorrowKind::Empty, BorrowKind::Empty) => Ordering::Equal,
            })
        }
    }

    impl Ord for BorrowKind {
        fn cmp(&self, other: &Self) -> std::cmp::Ordering {
            use std::cmp::Ordering;
            match (self, other) {
                (BorrowKind::Mut, BorrowKind::Mut) => Ordering::Equal,
                (BorrowKind::Mut, BorrowKind::Upgradable) => Ordering::Greater,
                (BorrowKind::Mut, BorrowKind::Ref) => Ordering::Greater,
                (BorrowKind::Mut, BorrowKind::Empty) => Ordering::Greater,
                (BorrowKind::Upgradable, BorrowKind::Mut) => Ordering::Less,
                (BorrowKind::Upgradable, BorrowKind::Upgradable) => Ordering::Equal,
                (BorrowKind::Upgradable, BorrowKind::Ref) => Ordering::Greater,
                (BorrowKind::Upgradable, BorrowKind::Empty) => Ordering::Greater,
                (BorrowKind::Ref, BorrowKind::Mut) => Ordering::Less,
                (BorrowKind::Ref, BorrowKind::Upgradable) => Ordering::Less,
                (BorrowKind::Ref, BorrowKind::Ref) => Ordering::Equal,
                (BorrowKind::Ref, BorrowKind::Empty) => Ordering::Greater,
                (BorrowKind::Empty, BorrowKind::Mut) => Ordering::Less,
                (BorrowKind::Empty, BorrowKind::Upgradable) => Ordering::Less,
                (BorrowKind::Empty, BorrowKind::Ref) => Ordering::Less,
                (BorrowKind::Empty, BorrowKind::Empty) => Ordering::Equal,
            }
        }
    }

    impl BorrowKind {
        pub fn would_wake(&self, other: &BorrowKind) -> bool {
            match (self, other) {
                (_, BorrowKind::Empty) => true,
                (BorrowKind::Upgradable | BorrowKind::Mut, _) => false,
                (BorrowKind::Empty, BorrowKind::Mut) => true,
                (_, BorrowKind::Mut) => false,
                (BorrowKind::Empty | BorrowKind::Ref, BorrowKind::Upgradable) => true,
                (_, BorrowKind::Ref) => true,
            }
        }

        pub fn is_exotic(&self) -> bool {
            match self {
                BorrowKind::Mut | BorrowKind::Upgradable => true,
                BorrowKind::Ref | BorrowKind::Empty => false,
            }
        }
    }

    struct BorrowWaker {
        kind: BorrowKind,
        waker: Waker,
    }

    impl BorrowWaker {
        pub fn wake(self) {
            self.waker.wake();
        }

        pub fn set_waker(&mut self, cx: &mut Context<'_>) {
            self.waker = cx.waker().clone();
        }
    }

    pub struct BorrowFuture {
        id: BorrowId,
        node: Arc<Mutex<BorrowQueueNode>>
    }

    impl Future for BorrowFuture {
        type Output = ();

        fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            match self.node.lock().unwrap().get_mut(&self.id) {
                Some(borrow_waker) => {
                    borrow_waker.set_waker(cx);
                    Poll::Pending
                },
                None => Poll::Ready(()),
            }
        }
    }

    /// If owner is None, then owner has been dropped or owner has been awoken, either way no communication between owner and borrowers is required.
    pub enum BorrowQueue {
        /// Only the owner is left awaiting.
        /// This is handled as a seprate state to recude indirection for simple cases.
        Final{ owner: Option<Waker> },
        /// This state is created when upgrading an upgref if the state is still `Forwarded`
        /// and is converted into a regular forwarded state when exotic count hits 0.
        Upgrading{ owner: Option<Waker>, upgrades: Arc<Mutex<BorrowQueueNode>> },
        /// Contains forwarded borrows to be handled in sequence, upon waking an upgref, create a new ForWarded 
        Forwarded{ owner: Option<Waker>, next: Arc<Mutex<BorrowQueueNode>> },
    }

    impl BorrowQueue {
        pub fn new() -> Self {
            BorrowQueue::Final { owner: None }
        }

        pub fn queue_upgrade(&mut self) -> BorrowFuture {
            match std::mem::replace(self, BorrowQueue::new()) {
                BorrowQueue::Final { owner } => *self = BorrowQueue::Upgrading {
                    owner,
                    upgrades: Arc::new(Mutex::new(BorrowQueueNode::new_upgrade_queue(None)))
                },
                BorrowQueue::Upgrading {..} => (),
                BorrowQueue::Forwarded { owner, next } => *self = BorrowQueue::Upgrading {
                    owner,
                    upgrades: Arc::new(Mutex::new(BorrowQueueNode::new_upgrade_queue(Some(next))))
                },
            };
            // self is now garunteed to be `BorrowQueue::Upgrading`
            let BorrowQueue::Upgrading { owner: _, upgrades } = self else {
                unsafe { std::hint::unreachable_unchecked() }
            };
            let this = upgrades.clone();
            // TODO: handle poisoning
            let mut node = upgrades.lock().unwrap();
            // there will be no followup as `BorrowKind::Mut` will never prompt an upgrade queue reduction as it is exotic.
            node.enqueue(this, BorrowKind::Mut).0
        }

        pub fn end_upgrading(&mut self) {
            match std::mem::replace(self, BorrowQueue::new()) {
                this@BorrowQueue::Final {..} => *self = this,
                BorrowQueue::Upgrading { owner, upgrades } => {
                    // TODO: handle panic
                    let mut node = upgrades.lock().unwrap();
                    // no longer an upgrade queue
                    node.last_interaction = LastInteraction::Blocked(BorrowKind::Ref);
                    node.status = NodeStatus::Shrinking;
                    drop(node);
                    *self = BorrowQueue::Forwarded { owner, next: upgrades };
                },
                this@BorrowQueue::Forwarded {..} => *self = this,
            }
        }

        #[must_use]
        pub fn wake_from(&mut self, mut current: BorrowKind, counts: &mut ContractCounts) -> AdditionalWork {
            loop {
                match std::mem::replace(self, BorrowQueue::new()) {
                    Self::Final { owner } => {
                        if let BorrowKind::Empty = current {
                            if let Some(waker) = owner {
                                counts.mut_number += 1;
                                waker.wake();
                                break;
                            } else {
                                return AdditionalWork::Drop;
                            }
                        } else {
                            *self = Self::Final { owner };
                        }
                        break;
                    }
                    Self::Upgrading { owner, upgrades } => {
                        // TODO: handle panic
                        let mut node = upgrades.lock().unwrap();
                        node.reduce_upgrade_buffer(counts);
                        drop(node);
                        *self = Self::Upgrading { owner, upgrades };
                        break;
                    }
                    Self::Forwarded { owner, next } => {
                        // TODO: handle panic
                        let mut node = next.lock().unwrap();
                        match node.waking_drain(&mut current, counts) {
                            WakeContinuation::Await => {
                                drop(node);
                                *self = Self::Forwarded { owner, next };
                                break;
                            },
                            WakeContinuation::Complete => {
                                *self = if let Some(next) = node.next.take() {
                                    Self::Forwarded { owner, next }
                                } else {
                                    Self::Final { owner }
                                };
                                break;
                            },
                            WakeContinuation::Next => {
                                *self = if let Some(next) = node.next.take() {
                                    Self::Forwarded { owner, next }
                                } else {
                                    Self::Final { owner }
                                };
                                continue;
                            },
                        }
                        #[allow(unreachable_code)]
                        {
                            unreachable!()
                        }
                    }
                }
                #[allow(unreachable_code)]
                {
                    unreachable!()
                }
            };
            AdditionalWork::Noop
        } 
    }
}

use borrow_queue::{BorrowId, BorrowKind, BorrowQueue};

pub mod states {
    use borrow_queue::BorrowKind;

    use super::*;

    pub trait PtrState: Sized {
        fn un_track(self, state: &Mutex<ContractState>) -> AdditionalWork {
            let _ = state;
            AdditionalWork::Noop
        }
    }
    pub unsafe trait RefState: PtrState {
        const BORROW_KIND: BorrowKind;

        fn release(self, _state: &mut ContractState) {}
    }
    impl<T: RefState> PtrState for T {
        fn un_track(self, state: &Mutex<ContractState>) -> AdditionalWork {
            // TODO: handle mutex poisoning
            let mut state = state.lock().unwrap();
            self.release(&mut*state);
            state.reduce_borrow(&Self::BORROW_KIND)
        }
    }
    pub unsafe trait RefMutState: RefState {}

    pub struct Empty;
    impl PtrState for Empty {}

    pub struct Owner;
    impl PtrState for Owner {}

    impl Owner {
        pub unsafe fn init() -> Self {
            Owner
        }
    }

    pub struct Host;
    impl PtrState for Host {}

    // u64 to ensure it can count is high enough
    #[derive(Clone)]
    pub struct Weak(pub u64);
    impl PtrState for Weak {}
    impl Weak {
        pub fn try_upgrade(self, mut_number: u64) -> Result<Ref, Empty> {
            if self.0 == mut_number {
                Ok(Ref)
            } else {
                Err(Empty)
            }
        }
    }

    #[derive(Clone)]
    pub struct Ref;
    unsafe impl RefState for Ref {
        const BORROW_KIND: BorrowKind = BorrowKind::Ref;
    }

    impl Ref {
        pub fn downgrade(self, mut_number: u64) -> Weak {
            Weak(mut_number)
        }
    }

    #[derive(Clone)]
    pub struct UpgradableRef;
    unsafe impl RefState for UpgradableRef {
        const BORROW_KIND: BorrowKind = BorrowKind::Upgradable;

        fn release(self, state: &mut ContractState) {
            if state.counts.exotic_count == 0 {
                state.wakers.end_upgrading();
            }
        }
    }

    impl UpgradableRef {
        pub fn into_ref(self) -> Ref {
            Ref
        }

        pub unsafe fn assert_unique(self) -> RefMut {
            RefMut
        }
    }

    pub struct RefMut;
    unsafe impl RefState for RefMut {
        const BORROW_KIND: BorrowKind = BorrowKind::Upgradable;
    }
    unsafe impl RefMutState for RefMut {}
    impl RefMut {
        pub fn into_ref(self) -> Ref {
            Ref
        }

        pub fn downgrade(self) -> UpgradableRef {
            UpgradableRef
        }
    }
}

pub struct Pointer<T, S: states::PtrState> {
    /// only valid to be uninit if about to forget
    inner: MaybeUninit<PointerInner<T, S>>
}

pub struct PointerInner<T, S: states::PtrState> {
    state: S,
    contract: NonNull<Contract<T>>,
}

impl<T, S: states::PtrState> Deref for Pointer<T, S> {
    type Target = PointerInner<T, S>;

    fn deref(&self) -> &Self::Target {
        unsafe { self.inner.assume_init_ref() }
    }
}

impl<T, S: states::PtrState> DerefMut for Pointer<T, S> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { self.inner.assume_init_mut() }
    }
}

impl<T, S: states::PtrState> Pointer<T, S> {
    fn into_inner(mut self) -> PointerInner<T, S> {
        let inner = unsafe { std::mem::replace(&mut self.inner, MaybeUninit::uninit()).assume_init() };
        std::mem::forget(self);
        inner
    }

    fn handle_drop(inner: PointerInner<T, S>) -> Self {
        Pointer {
            inner: MaybeUninit::new(inner)
        }
    }

    fn new_ptr<R: states::PtrState>(&self, state: R) -> Pointer<T, R> {
        self.contract().ref_count.fetch_add(1, Ordering::Relaxed);
        Pointer::handle_drop(PointerInner { state, contract: self.contract })
    }
}

impl<T, S: states::RefState> Pointer<T, S> {
    pub fn get_ref(&self) -> &T {
        unsafe { self.contract().value.get().as_ref().unwrap_unchecked().assume_init_ref() }
    }
}

impl<T, S: states::RefMutState> Pointer<T, S> {
    pub fn get_mut(&mut self) -> &mut T {
        unsafe { self.contract().value.get().as_mut().unwrap_unchecked().assume_init_mut() }
    }
}

impl<T> Pointer<T, states::Weak> {
    fn upgrade_inner(self) -> Result<Pointer<T, states::Ref>, Pointer<T, states::Empty>> {
        // TODO: handle panics
        let PointerInner { state, contract } = self.into_inner();
        let mut contract_state = unsafe { contract.as_ref().contract_state.lock().unwrap() };
        match state.try_upgrade(contract_state.counts.mut_number) {
            Ok(state) => {
                contract_state.cast_borrow(&BorrowKind::Empty, &states::Ref::BORROW_KIND);
                Ok(Pointer::handle_drop(PointerInner { state, contract }))
            },
            Err(state) => Err(Pointer::handle_drop(PointerInner { state, contract })),
        }
    }

    pub fn upgrade(self) -> Option<Pointer<T, states::Ref>> {
        self.upgrade_inner().ok()
    }
}

impl<T> Pointer<T, states::Ref> {
    pub fn downgrade(&self) -> Pointer<T, states::Weak> {
        // TODO: handle panics
        let mut_number = self.contract().contract_state.lock().unwrap().counts.mut_number;
        self.new_ptr(states::Weak(mut_number))
    }
}

impl<T> Pointer<T, states::UpgradableRef> {
    pub fn as_ref(&self) -> Pointer<T, states::Ref> {
        // TODO: handle panics
        self.contract().contract_state.lock().unwrap().counts.record_borrow_acquire(&states::Ref::BORROW_KIND);
        let ref_state = self.state.clone().into_ref();
        self.new_ptr(ref_state)
    }
}

impl<T> Pointer<T, states::RefMut> {
    pub fn into_ref(self) -> Pointer<T, states::Ref> {
        // TODO: handle panics
        let PointerInner { state, contract } = self.into_inner();
        let mut contract_state = unsafe { contract.as_ref().contract_state.lock().unwrap() };
        contract_state.cast_borrow(&states::RefMut::BORROW_KIND, &states::Ref::BORROW_KIND);
        drop(contract_state);
        Pointer::handle_drop(PointerInner { state: state.into_ref(), contract })
    }

    pub fn downgrade(self) -> Pointer<T, states::UpgradableRef> {
        // TODO: handle panics
        let PointerInner { state, contract } = self.into_inner();
        let mut contract_state = unsafe { contract.as_ref().contract_state.lock().unwrap() };
        // this cast will never prompt and wakes as future ptrs expect all potential mutation to cease until waking, including by upgradable refs
        contract_state.silent_cast_borrow(&states::RefMut::BORROW_KIND, &states::UpgradableRef::BORROW_KIND);
        drop(contract_state);
        Pointer::handle_drop(PointerInner { state: state.downgrade(), contract })
    }
}

impl<T, S: states::PtrState> Drop for Pointer<T, S> {
    fn drop(&mut self) {
        let PointerInner { state, contract } = unsafe { std::mem::replace(&mut self.inner, MaybeUninit::uninit()).assume_init() };
        match state.un_track(&unsafe { contract.as_ref() }.contract_state) {
            AdditionalWork::Noop => (),
            AdditionalWork::Drop => unsafe { contract.as_ref().value.get().as_mut().unwrap_unchecked().assume_init_drop() },
        }
        if unsafe { contract.as_ref() }.ref_count.fetch_sub(1, Ordering::Release) == 1 {
            drop(unsafe { Box::from_raw(contract.as_ptr()) })
        }
    }
}

struct PointerAwait<T, S: states::PtrState> {
    id: BorrowId,
    inner: Option<(PointerInner<T, S>, std::sync::Arc<Mutex<borrow_queue::BorrowQueueNode>>)>
}

impl<T, S: states::PtrState> PointerAwait<T, S> {
    pub fn is_pollable(&self) -> bool {
        self.inner.is_some()
    }

    pub fn poll_val(&mut self, cx: &mut Context) -> Poll<Pointer<T, S>> {
        // TODO: handle panics
        let (pointer, queue) = self.inner.as_mut().unwrap();
        let mut node = queue.lock().unwrap();
        let poll = node.poll_at(&self.id, cx);
        drop(node);
        poll.map(|_| {
            let _ = pointer;
            // safe as we would have already panicked if this was not the case
            Pointer::handle_drop(unsafe { self.inner.take().unwrap_unchecked() }.0)
        })
    }
}

impl<T, S: states::PtrState> Drop for PointerAwait<T, S> {
    fn drop(&mut self) {
        let Some((pointer, queue)) = self.inner.take() else {
            return;
        };
        // TODO: handle panic
        let mut node = queue.lock().unwrap();
        if let Some(()) = node.close_waker(&self.id) {
            drop(Pointer::handle_drop(pointer.map_state(|_| states::Empty)));
            return;
        } 
        drop(node);
        drop(Pointer::handle_drop(pointer))
    }
}

impl<T, S: states::PtrState> PointerInner<T, S> {
    fn contract(&self) -> &Contract<T> {
        unsafe { self.contract.as_ref() }
    }

    pub fn get_ref(&self) -> &T where S: states::RefState {
        unsafe { self.contract().value.get().as_ref().unwrap_unchecked().assume_init_ref() }
    }

    pub fn get_mut(&mut self) -> &mut T where S: states::RefMutState {
        unsafe { self.contract().value.get().as_mut().unwrap_unchecked().assume_init_mut() }
    }

    fn map_state<Q: states::PtrState>(self, f: impl FnOnce(S) -> Q) -> PointerInner<T, Q> {
        PointerInner {
            state: (f)(self.state),
            contract: self.contract
        }
    }
}

impl<T> Pointer<T, states::Owner> {
    pub fn init_with(value: T) -> Self {
        let contract = Contract {
            ref_count: 1.into(),
            value: UnsafeCell::new(MaybeUninit::new(value)),
            contract_state: Mutex::new(ContractState::new()),
        };
        let contract = unsafe { NonNull::new_unchecked(Box::into_raw(Box::new(contract))) };
        let state = unsafe { Owner::init() };
        Pointer::handle_drop(PointerInner { state, contract })
    }

    pub fn take_value(self) -> T {
        let value = unsafe { std::mem::replace(self.contract().value.get().as_mut().unwrap_unchecked(), MaybeUninit::uninit()).assume_init() };
        drop(Pointer::handle_drop(self.into_inner().map_state(|_| states::Empty)));
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vibe_check() {
        // ...
    }
}