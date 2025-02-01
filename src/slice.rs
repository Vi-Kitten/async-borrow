use std::ptr;

use futures::{FutureExt, Stream};

use crate::{Ref, RefMut, RefForward, RefMutForward};

impl<T, B> RefForward<T, [B]> {
    pub fn map_by_index(this: Self, index: usize) -> Option<RefForward<T, B>> {
        if index < this.borrow.len() {
            Some(RefForward {
                ptr: this.ptr,
                borrow: unsafe { (this.borrow as *mut B).add(index) },
            })
        } else {
            None
        }
    }

    pub fn map_by_range(this: Self, range: std::ops::Range<usize>) -> Option<RefForward<T, [B]>> {
        if range.end <= this.borrow.len() && range.start < this.borrow.len() {
            Some(RefForward {
                ptr: this.ptr,
                borrow: unsafe { ptr::slice_from_raw_parts_mut((this.borrow as *mut B).add(range.start), range.len()) }
            })
        } else {
            None
        }
    }

    pub fn map_first(this: Self) -> Option<RefForward<T, B>> {
        RefForward::map_by_index(this, 0)
    }

    pub fn map_skip_first(this: Self) -> Option<RefForward<T, [B]>> {
        let len = this.borrow.len();
        RefForward::map_by_range(this, 1..len)
    }
}

impl<T, B> Ref<T, [B]> {
    pub fn get(this: Self, index: usize) -> Option<Ref<T, B>> {
        this.scope(move |this, ctx| {
            this.get(index).map(|x| ctx.lift_ref(x))
        })
    }

    pub fn range(this: Self, range: std::ops::Range<usize>) -> Option<Ref<T, [B]>> {
        this.scope(move |this, ctx| {
            this.get(range).map(|x| ctx.lift_ref(x))
        })
    }

    pub fn first(this: Self) -> Option<Ref<T, B>> {
        Ref::get(this, 0)
    }

    pub fn skip_first(this: Self) -> Option<Ref<T, [B]>> {
        let len = this.len();
        Ref::range(this, 1..len)
    }

    pub fn split_first(this: Self) -> Option<(Ref<T, B>, Ref<T, [B]>)> {
        this.scope(move |this, ctx| {
            let (x, xs) = this.split_first()?;
            Some((ctx.lift_ref(x), ctx.lift_ref(xs)))
        })
    }

    pub fn split_at(this: Self, mid: usize) -> (Ref<T, [B]>, Ref<T, [B]>) {
        this.scope(move |this, ctx| {
            let (x, y) = this.split_at(mid);
            (ctx.lift_ref(x), ctx.lift_ref(y))
        })
    }

    pub fn chunk_by<F: for<'a> FnMut(&'a B, &'a B) -> bool>(this: Self, f: F) -> ChunkBy<T, B, F> {
        ChunkBy { rf: Some(this), f }
    }

    pub fn chunks(this: Self, size: usize) -> Chunks<T, B> {
        Chunks { rf: Some(this), n: size }
    }

    pub fn split<F: for<'a> FnMut(&'a B) -> bool>(this: Self, f: F) -> Split<T, B, F> {
        Split { rf: Some(this), f }
    }

    pub fn split_inclusive<F: for<'a> FnMut(&'a B) -> bool>(this: Self, f: F) -> SplitInclusive<T, B, F> {
        SplitInclusive { rf: Some(this), f }
    }

    pub fn windows(this: Self, size: usize) -> Windows<T, B> {
        Windows { rf: Some(this), n: size }
    }
}

pub struct ChunkBy<T, B, F: for<'a> FnMut(&'a B, &'a B) -> bool> {
    rf: Option<Ref<T, [B]>>,
    f: F
}

impl<T, B, F: for<'a> FnMut(&'a B, &'a B) -> bool> Iterator for ChunkBy<T, B, F> {
    type Item = Ref<T, [B]>;

    fn next(&mut self) -> Option<Self::Item> {
        let rf = self.rf.take()?;
        let mut next_pair_start = 0;
        while let Some([x, y]) = rf[next_pair_start..].first_chunk::<2>() {
            next_pair_start += 1;
            if !(self.f)(x, y) {
                let (rf_lft, rf_rgh) = Ref::split_at(rf, next_pair_start);
                self.rf = Some(rf_rgh);
                return Some(rf_lft)
            }
        };
        Some(rf)
    }
}

pub struct Chunks<T, B> {
    rf: Option<Ref<T, [B]>>,
    n: usize,
}

impl<T, B> Iterator for Chunks<T, B> {
    type Item = Ref<T, [B]>;

    fn next(&mut self) -> Option<Self::Item> {
        let rf = self.rf.take()?;
        if self.n >= rf.len() {
            return Some(rf)
        }
        let (rf_lft, rf_rgh) = Ref::split_at(rf, self.n);
        self.rf = Some(rf_rgh);
        Some(rf_lft)
    }
}

pub struct SplitInclusive<T, B, F: for<'a> FnMut(&'a B) -> bool> {
    rf: Option<Ref<T, [B]>>,
    f: F,
}

impl<T, B, F: for<'a> FnMut(&'a B) -> bool> Iterator for SplitInclusive<T, B, F> {
    type Item = Ref<T, [B]>;

    fn next(&mut self) -> Option<Self::Item> {
        let rf = self.rf.take()?;
        let mut n = 0;
        while let Some(x) = rf.get(n) {
            n += 1;
            if (self.f)(x) {
                let (rf_lft, rf_rgh) = Ref::split_at(rf, n);
                self.rf = Some(rf_rgh);
                return Some(rf_lft)
            }
        }
        return Some(rf)
    }
}

pub struct Split<T, B, F: for<'a> FnMut(&'a B) -> bool> {
    rf: Option<Ref<T, [B]>>,
    f: F
}

impl<T, B> RefMutForward<T, [B]> {
    pub fn map_by_index_mut(this: Self, index: usize) -> Option<RefMutForward<T, B>> {
        if index < this.borrow.len() {
            Some(RefMutForward {
                ptr: this.ptr,
                borrow: unsafe { (this.borrow as *mut B).add(index) },
            })
        } else {
            None
        }
    }

    pub fn map_by_range_mut(this: Self, range: std::ops::Range<usize>) -> Option<RefMutForward<T, [B]>> {
        if range.end <= this.borrow.len() && range.start < this.borrow.len() {
            Some(RefMutForward {
                ptr: this.ptr,
                borrow: unsafe { ptr::slice_from_raw_parts_mut((this.borrow as *mut B).add(range.start), range.len()) }
            })
        } else {
            None
        }
    }

    pub fn map_first_mut(this: Self) -> Option<RefMutForward<T, B>> {
        RefMutForward::map_by_index_mut(this, 0)
    }

    pub fn map_skip_first_mut(this: Self) -> Option<RefMutForward<T, [B]>> {
        let len = this.borrow.len();
        RefMutForward::map_by_range_mut(this, 1..len)
    }
}

impl<T, B, F: for<'a> FnMut(&'a B) -> bool> Iterator for Split<T, B, F> {
    type Item = Ref<T, [B]>;

    fn next(&mut self) -> Option<Self::Item> {
        let rf = self.rf.take()?;
        let mut n = 0;
        while let Some(x) = rf.get(n) {
            if (self.f)(x) {
                let (rf_lft, rf_rgh) = Ref::split_at(rf, n);
                self.rf = Ref::skip_first(rf_rgh);
                return Some(rf_lft)
            }
            n += 1;
        }
        return Some(rf)
    }
}

pub struct Windows<T, B> {
    rf: Option<Ref<T, [B]>>,
    n: usize,
}

impl<T, B> Iterator for Windows<T, B> {
    type Item = Ref<T, [B]>;

    fn next(&mut self) -> Option<Self::Item> {
        let rf = self.rf.take()?;
        let xs = Ref::range(rf.clone(), 0..self.n)?;
        self.rf = Ref::skip_first(rf);
        Some(xs)
    }
}

impl<T, B> RefMut<T, [B]> {
    pub fn get_mut(this: Self, index: usize) -> Option<RefMut<T, B>> {
        this.scope(move |this, ctx| {
            this.get_mut(index).map(|x| ctx.lift_mut(x))
        })
    }

    pub fn range_mut(this: Self, range: std::ops::Range<usize>) -> Option<RefMut<T, [B]>> {
        this.scope(move |this, ctx| {
            this.get_mut(range).map(|x| ctx.lift_mut(x))
        })
    }

    pub fn first_mut(this: Self) -> Option<RefMut<T, B>> {
        RefMut::get_mut(this, 0)
    }

    pub fn skip_first_mut(this: Self) -> Option<RefMut<T, [B]>> {
        let len = this.len();
        RefMut::range_mut(this, 1..len)
    }

    pub fn split_first_mut(this: Self) -> Option<(RefMut<T, B>, RefMut<T, [B]>)> {
        this.scope(move |this, ctx| {
            let (x, xs) = this.split_first_mut()?;
            Some((ctx.lift_mut(x), ctx.lift_mut(xs)))
        })
    }

    pub fn split_at_mut(this: Self, mid: usize) -> (RefMut<T, [B]>, RefMut<T, [B]>) {
        this.scope(move |this, ctx| {
            let (x, y) = this.split_at_mut(mid);
            (ctx.lift_mut(x), ctx.lift_mut(y))
        })
    }

    pub fn chunk_by_mut<F: for<'a> FnMut(&'a mut B, &'a mut B) -> bool>(this: Self, f: F) -> ChunkByMut<T, B, F> {
        ChunkByMut { rf_mut: Some(this), f }
    }

    pub fn chunks_mut(this: Self, size: usize) -> ChunksMut<T, B> {
        ChunksMut { rf_mut: Some(this), n: size }
    }

    pub fn split_mut<F: for<'a> FnMut(&'a mut B) -> bool>(this: Self, f: F) -> SplitMut<T, B, F> {
        SplitMut { rf_mut: Some(this), f }
    }

    pub fn split_inclusive_mut<F: for<'a> FnMut(&'a mut B) -> bool>(this: Self, f: F) -> SplitInclusiveMut<T, B, F> {
        SplitInclusiveMut { rf_mut: Some(this), f }
    }

    pub fn windows_mut(this: Self, size: usize) -> WindowsMut<T, B> {
        WindowsMut { fut: Some(this.into_mut_forward()), size }
    }

    pub fn window_clusters_mut(this: Self, size: usize) -> WindowClustersMut<T, B> {
        WindowClustersMut { fut: Some(this.into_mut_forward()), size, n: 0 }
    }
}

pub struct ChunkByMut<T, B, F: for<'a> FnMut(&'a mut B, &'a mut B) -> bool> {
    rf_mut: Option<RefMut<T, [B]>>,
    f: F
}

impl<T, B, F: for<'a> FnMut(&'a mut B, &'a mut B) -> bool> Iterator for ChunkByMut<T, B, F> {
    type Item = RefMut<T, [B]>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut rf_mut = self.rf_mut.take()?;
        let mut next_pair_start = 0;
        while let Some([x, y]) = rf_mut[next_pair_start..].first_chunk_mut::<2>() {
            next_pair_start += 1;
            if !(self.f)(x, y) {
                let (rf_mut_lft, rf_mut_rgh) = RefMut::split_at_mut(rf_mut, next_pair_start);
                self.rf_mut = Some(rf_mut_rgh);
                return Some(rf_mut_lft)
            }
        };
        Some(rf_mut)
    }
}

pub struct ChunksMut<T, B> {
    rf_mut: Option<RefMut<T, [B]>>,
    n: usize,
}

impl<T, B> Iterator for ChunksMut<T, B> {
    type Item = RefMut<T, [B]>;

    fn next(&mut self) -> Option<Self::Item> {
        let rf_mut = self.rf_mut.take()?;
        if self.n >= rf_mut.len() {
            return Some(rf_mut)
        }
        let (rf_mut_lft, rf_mut_rgh) = RefMut::split_at_mut(rf_mut, self.n);
        self.rf_mut = Some(rf_mut_rgh);
        Some(rf_mut_lft)
    }
}

pub struct SplitInclusiveMut<T, B, F: for<'a> FnMut(&'a mut B) -> bool> {
    rf_mut: Option<RefMut<T, [B]>>,
    f: F,
}

impl<T, B, F: for<'a> FnMut(&'a mut B) -> bool> Iterator for SplitInclusiveMut<T, B, F> {
    type Item = RefMut<T, [B]>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut rf_mut = self.rf_mut.take()?;
        let mut n = 0;
        while let Some(x) = rf_mut.get_mut(n) {
            n += 1;
            if (self.f)(x) {
                let (rf_mut_lft, rf_mut_rgh) = RefMut::split_at_mut(rf_mut, n);
                self.rf_mut = Some(rf_mut_rgh);
                return Some(rf_mut_lft)
            }
        }
        return Some(rf_mut)   
    }
}

pub struct SplitMut<T, B, F: for<'a> FnMut(&'a mut B) -> bool> {
    rf_mut: Option<RefMut<T, [B]>>,
    f: F
}

impl<T, B, F: for<'a> FnMut(&'a mut B) -> bool> Iterator for SplitMut<T, B, F> {
    type Item = RefMut<T, [B]>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut rf_mut = self.rf_mut.take()?;
        let mut n = 0;
        while let Some(x) = rf_mut.get_mut(n) {
            if (self.f)(x) {
                let (rf_mut_lft, rf_mut_rgh) = RefMut::split_at_mut(rf_mut, n);
                self.rf_mut = RefMut::skip_first_mut(rf_mut_rgh);
                return Some(rf_mut_lft)
            }
            n += 1;
        }
        return Some(rf_mut)
    }
}

pub struct WindowsMut<T, B> {
    fut: Option<RefMutForward<T, [B]>>,
    size: usize,
}

impl<T, B> Stream for WindowsMut<T, B> {
    type Item = RefMut<T, [B]>;

    fn poll_next(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        let Some(mut fut) = self.fut.take() else { return Poll::Ready(None) };
        match fut.poll_unpin(cx) {
            Poll::Ready(rf_mut) => {
                let (fut, rf_mut) = rf_mut.forward_mut();
                let Some(xs) = RefMut::range_mut(rf_mut, 0..self.size) else {
                    return Poll::Ready(None)
                };
                self.fut = RefMutForward::map_skip_first_mut(fut);
                Poll::Ready(Some(xs))
            },
            Poll::Pending => {
                self.fut = Some(fut);
                Poll::Pending
            },
        }
    }
}

pub struct WindowClustersMut<T, B> {
    fut: Option<RefMutForward<T, [B]>>,
    size: usize,
    n: usize
}

impl<T, B> Stream for WindowClustersMut<T, B> {
    type Item = ChunksMut<T, B>;

    fn poll_next(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        let Some(mut fut) = self.fut.take() else { return Poll::Ready(None) };
        if self.n == self.size {
            return Poll::Ready(None)
        }
        match fut.poll_unpin(cx) {
            Poll::Ready(rf_mut) => {
                self.n += 1;
                let (fut, rf_mut) = rf_mut.forward_mut();
                self.fut = RefMutForward::map_skip_first_mut(fut);
                Poll::Ready(Some(RefMut::chunks_mut(rf_mut, self.size)))
            },
            Poll::Pending => {
                self.fut = Some(fut);
                Poll::Pending
            },
        }
    }
}