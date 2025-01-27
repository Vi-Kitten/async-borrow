# TODO

- [ ] Impl all the traits that `Box`, `Arc`, ect would.
- [x] Allow for dereferencing within the async-borrow system.
    - [ ] Create a macro to provide destructuring support.
- [ ] Create an Object type that is an aligned pointer using that aligned bits to store share information.
    - [ ] Make an AtomicObject type.
- [ ] Handle pinning.
- [ ] Create `Ref` and `RefMut` compatable async lock primitives.
    - [ ] Have `Context` support these with extra methods.
