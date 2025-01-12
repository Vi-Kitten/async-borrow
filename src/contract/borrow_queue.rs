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

    pub fn owner_waker(&mut self) -> &mut Option<Waker> {
        match self {
            BorrowQueue::Final { owner } => owner,
            BorrowQueue::Upgrading { owner, upgrades: _ } => owner,
            BorrowQueue::Forwarded { owner, next: _ } => owner,
        }
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