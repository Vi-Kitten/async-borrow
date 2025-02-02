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

    unsafe fn free(free: &mut usize, node: &mut Node, index: usize) -> usize {
        node.branch.assume_init_drop();
        node.flush_weak_refs();
        let parent = std::mem::replace(&mut node.next, index);
        std::mem::swap(free, &mut node.next);
        parent
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
    /// 
    /// If true, if is your responsiblity to free the shared data.
    pub unsafe fn untrack_borrow_unchecked(&mut self, mut index: usize) -> bool {
        loop {
            if index == END {
                break true;
            }
            let node = self.nodes.get_unchecked_mut(index);
            let branch = node.branch.assume_init_mut();
            branch.strong -= 1;
            if branch.strong == 0 {
                match branch.waker.take() {
                    Some(waker) => {
                        waker.wake();
                        break false;
                    },
                    None => {
                        index = Graph::free(&mut self.free, node, index);
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
    /// 
    /// You are now responsible for maintaining a borrow pointer with the given index.
    pub unsafe fn poll_unchecked(&mut self, index: usize, cx: &mut Context<'_>) -> Poll<usize> {
        let node = self.nodes.get_unchecked_mut(index);
        let branch = node.branch.assume_init_mut();
        match branch.waker.as_mut() {
            Some(waker) => {
                if !waker.will_wake(cx.waker()) {
                    *waker = cx.waker().clone();
                }
                Poll::Pending
            },
            None => {
                let parent = Graph::free(&mut self.free, node, index);
                Poll::Ready(parent)
            },
        }
    }

    /// Returns true if root is freed.
    /// 
    /// If true, if is your responsiblity to free the shared data.
    pub unsafe fn close_future_unchecked(&mut self, index: usize) -> bool {
        let node = self.nodes.get_unchecked_mut(index);
        let branch = node.branch.assume_init_mut();
        match branch.waker.take() {
            Some(..) => false,
            None => {
                let parent = Graph::free(&mut self.free, node, index);
                unsafe { self.untrack_borrow_unchecked(parent) }
            },
        }
    }
}