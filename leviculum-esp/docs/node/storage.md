# `node::storage` — the shared flash handle

`src/node/storage.rs`

`esp-storage`'s `FlashStorage` is one object, but two writers exist:
the PID app saves config from the main loop, and the Blink app's
persist task writes from its own Embassy task. The handle therefore
lives in a static behind a blocking mutex:

```rust
type SharedFlash = Mutex<CriticalSectionRawMutex, RefCell<FlashStorage<'static>>>;
```

- `share(flash) -> &'static SharedFlash` — call once, after the
  identity has been loaded.
- `storage::with(&shared, f)` — run `f` with exclusive access. Every
  flash operation is a short synchronous call made inside the critical
  section.

`Ctx::with_flash` is the same helper re-exported for apps.
