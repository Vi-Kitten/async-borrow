# TODO

- [ ] Make slice methods return results on failure.
- [ ] Vibe check `Weak`.
- [ ] Take effort to ensure that slice methods match those in std lib.
- [ ] Split `scope` into a regular context lifting thing and one that handles async and spawning.
- [ ] Implement `Borrow` trait things.
- [ ] Make a `Stream` built by `ShareBox::share_while` and another from `RefMut::forward_while`.
- [ ] Integrate with concepts from the borrow trait.
- [ ] Impl all the traits that `Box`, `Arc`, ect would.
- [x] Allow for dereferencing within the async-borrow system.
    - [ ] Create a macro to provide destructuring support.
- [ ] Create an Object type that is an aligned pointer using that aligned bits to store share information.
    - [ ] Make an AtomicObject type.
- [ ] Handle pinning.
- [ ] Create `Ref` and `RefMut` compatable async lock primitives.
    - [ ] Have `Context` support these with extra methods.
