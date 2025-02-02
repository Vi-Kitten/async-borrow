# TODO

- [x] Make graph waking take on wake.
- [ ] Make slice methods return results on failure.
- [ ] Vibe check `Weak`.
- [ ] Take effort to ensure that slice methods match those in std lib.
- [ ] Split `scope` into a regular context lifting thing and one that handles async and spawning.
- [x] Implement `Borrow` trait things.
- [ ] Make a `Stream` built by `RefMutShare::share_while(&mut self, ...)` and another from `RefMutForward::forward_while(&mut self, ...)`.
- [ ] Impl all the traits that `Box`, `Arc`, ect would.
- [x] Allow for dereferencing within the async-borrow system.
    - [ ] Create a macro to provide destructuring support.
- [ ] Handle pinning.
- [ ] Create `Ref` and `RefMut` compatable async lock primitives.
    - [ ] Have `Context` support these with extra methods.
- [ ] Consider adding a global context.