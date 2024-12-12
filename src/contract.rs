use std::{cell::UnsafeCell, ptr::NonNull, sync::{atomic::{AtomicUsize, Ordering}, Mutex, PoisonError}, task::{Context, Poll, Waker}};

use states::Owner;

struct Contract<T> {
    /// The number of refereces pointing to the contract (includes weak references and owner).
    ///
    /// Weak ref upgrading requires incrementing `locking_count`
    /// this will fix the count to 2+ to allowing the ref to check the mut_number.
    ///
    /// Owner can only take when this is zero.
    ref_count: AtomicUsize,
    /// The number of references that could upgrade to become mutable (or are already mutable).
    ///
    /// When `exotic_count == 0` multiple immutable borrows can be awoken, optionally ending in waking an upgradable borrow.
    exotic_count: AtomicUsize,
    /// The number of references held.
    ///
    /// If 0 then reference is locking.
    ///
    /// Can be fixed to 2+ to ensure read only or owner only access to certain fields.
    borrow_count: AtomicUsize,
    /// Stores the shared data.
    ///
    /// Will always be `Some` until a `Final` contract holder calls `finalize`.
    ///
    /// Access:
    /// - `borrow_count == 0`: drop access (only if owner has detached).
    /// - `borrow_count > 0`: read / write depending on reference privillages.
    value: UnsafeCell<Option<Poisonable<T>>>,
    /// Number of mutable borrows thusfar (counts upgrading and owner re-aquisition).
    ///
    /// A weak ref can only upgrade if the `mut_number` has not increased.
    ///
    /// Access:
    /// - `borrow_count == 0`: locker can mutate.
    /// - `borrow_count > 0`: read.
    mut_number: UnsafeCell<usize>,
    wakers: UnsafeCell<hashheap::HashHeap<BorrowKey, WakerState>>,
}

struct Poisonable<T> {
    pub inner: T,
    is_poisoned: bool
}

impl<T> Poisonable<T> {
    pub fn new(value: T) -> Self {
        Poisonable { inner: value, is_poisoned: false }
    }

    pub fn is_poisoned(&self) -> bool {
        self.is_poisoned
    }

    pub fn poison(&mut self) {
        self.is_poisoned = true;
    }

    pub fn detox(&mut self) {
        self.is_poisoned = false;
    }
}

#[derive(PartialEq, Eq, Hash)]
struct BorrowKey {
    /// number of forwards deep
    level: usize,
    /// position in queue
    position: usize,
}

impl PartialOrd for BorrowKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        use std::cmp::Ordering::*;
        match self.level.cmp(&other.level) {
            Equal => /* flipped on purpose */ other.position.partial_cmp(&self.position),
            ord => Some(ord),
        }
    }
}

impl Ord for BorrowKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering::*;
        match self.level.cmp(&other.level) {
            Equal => /* flipped on purpose */ other.position.cmp(&self.position),
            ord => ord,
        }
    }
}

pub enum WakerState {
    Ref(Waker),
    UpRef(Waker),
    RefMut(Waker),
    Awoken,
    Acknowledged,
}

mod waker_store {
    use super::*;
    struct QueueStackEntry {
        state: WakerState,
        /// The offset to the start of the queue, is 0 if is the first element added
        start_offset: usize,
        // id: usize
    }
    
    impl QueueStackEntry {
        pub fn wake(&mut self) {
            match std::mem::replace(&mut self.state, WakerState::Awoken) {
                WakerState::Ref(waker) => waker.wake(),
                WakerState::UpRef(waker) => waker.wake(),
                WakerState::RefMut(waker) => waker.wake(),
                WakerState::Awoken => (),
                WakerState::Acknowledged => ()
            }
        }
    
        pub fn poll(&mut self, cx: Context) -> Poll<()> {
            match &mut self.state {
                WakerState::Ref(waker) => *waker = cx.waker().clone(),
                WakerState::UpRef(waker) => *waker = cx.waker().clone(),
                WakerState::RefMut(waker) => *waker = cx.waker().clone(),
                WakerState::Awoken => return Poll::Ready(()),
                WakerState::Acknowledged => return Poll::Ready(())
            }
            Poll::Pending
        }
    }
    
    pub struct BorrowQueueStack {
        current_queue_len: usize,
        entries: Vec<QueueStackEntry>
    }
    
    impl BorrowQueueStack {
        pub fn new() -> Self {
            BorrowQueueStack {
                current_queue_len: 0,
                entries: Vec::new()
            }
        }

        fn enqueue(&mut self, state: WakerState) -> usize {
            let n = self.entries.len();
            self.entries.push(QueueStackEntry { state, start_offset: self.current_queue_len });
            self.current_queue_len += 1;
            n
        }

        fn end_queue(&mut self) {
            self.current_queue_len = 0;
        }

        fn push(&mut self, state: WakerState) -> usize {
            let n = self.entries.len();
            self.entries.push(QueueStackEntry { state, start_offset: 0 });
            n
        }

        fn wake_any(&mut self) -> WakeInfo {
            todo!()
        }

        fn wake_refs(&mut self) -> WakeInfo {
            let mut info = WakeInfo {
                terminator: WakeTerminator::Drained,
                borrow_count: 0
            };
            let mut after_queue_index = self.entries.len();
            'queue_loop: loop {
                let Some(queue_index) = after_queue_index.checked_sub(1) else {
                    return info;
                };
                let last = unsafe { self.entries.get_unchecked(queue_index) };
                let mut entry_index = queue_index - last.start_offset;
                match &last.state {
                    WakerState::Ref(_) | WakerState::UpRef(_) | WakerState::RefMut(_) => (),
                    WakerState::Awoken | WakerState::Acknowledged => {
                        after_queue_index = entry_index;
                        continue 'queue_loop;
                    },
                }
                let _ = last;
                'entry_loop: while entry_index <= queue_index {

                    entry_index += 1;
                }

            }
            info
        }
    }
    
    pub enum WakerState {
        Ref(Waker),
        UpRef(Waker),
        RefMut(Waker),
        Awoken,
        Acknowledged,
    }

    pub enum WakeTerminator {
        FoundUpRef,
        FoundMut,
        Drained,
    }

    pub struct WakeInfo {
        terminator: WakeTerminator,
        borrow_count: usize,
    }
}

use waker_store::BorrowQueueStack;

mod states {
    pub struct Owner;
    impl State for Owner {}
    unsafe impl RefState for Owner {}
    unsafe impl RefMutState for Owner {}
    impl Owner {
        pub unsafe fn init() -> Owner {
            Owner
        }
    }
    pub struct Weak(usize);
    impl State for Weak {}
    impl Weak {
        pub fn try_upgrade(self, mut_number: usize) -> Option<Ref> {
            if self.0 == mut_number {
                Some(Ref)
            } else {
                None
            }
        }
    }
    pub struct Ref;
    impl State for Ref {}
    unsafe impl RefState for Ref {}
    impl Ref {
        pub fn downgrade(self, mut_number: usize) -> Weak {
            Weak(mut_number)
        }

        pub unsafe fn assert_unique(self) -> RefMut {
            RefMut
        }
    }
    pub struct RefMut;
    impl State for RefMut {}
    unsafe impl RefState for RefMut {}
    unsafe impl RefMutState for RefMut {}
    impl RefMut {
        pub fn downgrade(self) -> Ref {
            Ref
        }
    }
    pub struct Awaiting<S: RefState>(usize, S);
    impl<S: RefState> State for Awaiting<S> {}
    impl<S: RefState> Awaiting<S> {
        pub fn sleep(waker_index: usize, state: S) -> Awaiting<S> {
            Awaiting(waker_index, state)
        }

        pub fn waker_index(&self) -> usize {
            self.0
        }

        pub fn state_map<T: RefState>(self, f: impl FnOnce(S) -> T) -> Awaiting<T> {
            Awaiting(self.0, (f)(self.1))
        }

        pub unsafe fn into_awake(self) -> S {
            self.1
        }
    }
    pub trait State {}

    pub unsafe trait RefState: State {}
    pub unsafe trait RefMutState: RefState {}
}

struct ContractHolder<T, S: states::State> {
    state: S,
    ptr: NonNull<Contract<T>>,
}

impl<T, S: states::State> ContractHolder<T, S> {
    fn contract(&self) -> &Contract<T> {
        unsafe { self.ptr.as_ref() }
    }
}

impl<T> ContractHolder<T, states::Owner> {
    pub fn init_with(value: T) -> Self {
        let contract = Contract {
            ref_count: 1.into(),
            exotic_count: 0.into(),
            borrow_count: 0.into(),
            value: UnsafeCell::new(Some(Poisonable::new(value))),
            mut_number: UnsafeCell::new(0),
            wakers: UnsafeCell::new(hashheap::HashHeap::new_maxheap()),
        };
        let ptr = unsafe { NonNull::new_unchecked(Box::into_raw(Box::new(contract))) };
        let state = unsafe { Owner::init() };
        ContractHolder { state, ptr }
    }

    pub fn take_value(self) -> T {
        unsafe { self.contract().value.get().as_mut().unwrap_unchecked().take().unwrap_unchecked().inner }
    }
}

impl<T, S: states::State> Drop for ContractHolder<T, S> {
    fn drop(&mut self) {
        if self.contract().ref_count.fetch_sub(1, Ordering::Release) == 1 {
            drop(unsafe { Box::from_raw(self.ptr.as_ptr()) })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_borrow_key_ordering() {
        let x = BorrowKey {
            level: 0,
            position: 0,
        };
    }
}