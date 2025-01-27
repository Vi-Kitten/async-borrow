use std::{mem::MaybeUninit, task::{Context, Poll, Waker}, usize};

/// A branch in the borrowing chain, used to record forwards / shares.
struct Branch {
    /// A waker to be polled when either count reached 0.
    /// 
    /// Spurious polling is used as the wake condition of the forwarder is not known.
    /// 
    /// The waker is only `None` if waking is cancelled by the forwarder.
    waker: Option<Waker>,
    /// Number of Ref's + RefMut's.
    strong: usize,
}

struct Node {
    /// The node this was forwarded from / the next free node.
    next: usize,
    /// Two times the number of previous weak references, is odd if currently being referenced weakly.
    version: u64,
    branch: MaybeUninit<Branch>,
}

impl Node {
    pub fn new_free_chain(len: usize) -> Box<[Node]> {
        (0..len).map(|n| Node {
            next: n + 1,
            version: 0,
            branch: MaybeUninit::uninit()
        }).collect()
    }

    fn log_weak_ref(&mut self) -> u64 {
        // round up to next odd number
        self.version |= 1;
        self.version
    }

    fn flush_weak_refs(&mut self) {
        // round up to next even number
        self.version += 1;
        self.version &= -2_i64 as u64;
    }
}

pub const END: usize = usize::MAX;

pub struct Graph {
    free: usize,
    nodes: Box<[Node]>
}

impl Graph {
    pub fn new() -> Graph {
        Graph::with_capacity(7)
    }

    pub fn with_capacity(capacity: usize) -> Graph {
        Graph {
            free: 0,
            nodes: Node::new_free_chain(capacity),
        }
    }

    fn alloc(&mut self, branch: Branch) -> &mut Node {
        if self.free == self.nodes.len() {
            let next_len = (self.nodes.len() << 1) + 1;
            let allocated = std::mem::replace(&mut self.nodes, Node::new_free_chain(next_len));
            for (i, node) in allocated.into_vec().into_iter().enumerate() {
                *unsafe { self.nodes.get_unchecked_mut(i) } = node;
            }
        }
        let node = unsafe { self.nodes.get_unchecked_mut(self.free) };
        std::mem::swap(&mut node.next, &mut self.free);
        node.branch.write(branch);
        node
    }

    unsafe fn free(free: &mut usize, node: &mut Node) {
        node.branch.assume_init_drop();
        node.flush_weak_refs();
        std::mem::swap(free, &mut node.next);
    }

    pub fn share(&mut self) -> usize {
        self.forward(END)
    }

    pub fn forward(&mut self, parent: usize) -> usize {
        let node = self.alloc(Branch {
            waker: Some(futures::task::noop_waker()),
            strong: 1,
        });
        std::mem::replace(&mut node.next, parent)
    }

    pub unsafe fn track_weak_unchecked(&mut self, index: usize) -> u64 {
        self.nodes.get_unchecked_mut(index).log_weak_ref()
    }

    /// Returns true on success.
    pub unsafe fn try_upgrade_weak_unchecked(&mut self, index: usize, version: u64) -> bool {
        let node = self.nodes.get_unchecked_mut(index);
        if node.version != version {
            return false;
        }
        let branch = node.branch.assume_init_mut();
        branch.strong += 1;
        true
    }

    pub unsafe fn track_borrow_unchecked(&mut self, index: usize) {
        let branch = self.nodes.get_unchecked_mut(index).branch.assume_init_mut();
        branch.strong += 1;
    }

    /// Returns true if root is freed.
    pub unsafe fn untrack_borrow_unchecked(&mut self, mut index: usize) -> bool {
        loop {
            if index == END {
                break true;
            }
            let node = self.nodes.get_unchecked_mut(index);
            let branch = node.branch.assume_init_mut();
            branch.strong -= 1;
            if branch.strong == 0 {
                match branch.waker.as_mut() {
                    Some(waker) => {
                        std::mem::replace(waker, futures::task::noop_waker()).wake();
                        break false;
                    },
                    None => {
                        debug_assert_eq!(node.next, index, "Node must point to self before free!");
                        index = std::mem::replace(&mut node.next, index);
                        Graph::free(&mut self.free, node);
                        continue;
                    },
                }
            } else {
                break false;
            }
            #[allow(unreachable_code)]
            {
                unreachable!()
            }
        }
    }

    /// Returns the parent index.
    pub unsafe fn poll_unchecked(&mut self, index: usize, cx: &mut Context<'_>) -> Poll<usize> {
        let node = self.nodes.get_unchecked_mut(index);
        let branch = node.branch.assume_init_mut();
        debug_assert!(branch.waker.is_some(), "An open future must have a set waker!");
        if branch.strong == 0 {
            let parent = std::mem::replace(&mut node.next, index);
            Graph::free(&mut self.free, node);
            Poll::Ready(parent)
        } else {
            if !branch.waker.as_ref().unwrap_unchecked().will_wake(cx.waker()) {
                branch.waker = Some(cx.waker().clone())
            }
            Poll::Pending
        }
    }

    /// Is only true is closure was successful.
    pub unsafe fn close_future_unchecked(&mut self, index: usize) -> bool {
        let node = self.nodes.get_unchecked_mut(index);
        let branch = node.branch.assume_init_mut();
        debug_assert!(branch.waker.is_some(), "An open future must have a set waker!");
        if branch.strong == 0 {
            // assume it has been left in a state to free
            Graph::free(&mut self.free, node);
            false
        } else {
            branch.waker = None;
            true
        }
    }
}