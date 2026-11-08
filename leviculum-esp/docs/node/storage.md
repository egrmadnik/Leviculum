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

## Who writes, and when

```
           main loop (app)                    app task
 ┌────────────────────────────┐    ┌──────────────────────────────┐
 │ pid_app: save_config       │    │ blink: persist_task           │
 │   (remote tune → flash)    │    │   (debounced 5 s → save_blink)│
 └─────────────┬──────────────┘    └───────────────┬──────────────┘
               │  storage::with(flash, …)          │
               ▼                                   ▼
        ┌──────────────────────────────────────────────────┐
        │  SharedFlash = Mutex<RefCell<FlashStorage>>       │
        │  one object, exclusive access per op             │
        └──────────────────────────────────────────────────┘
```

The flash slot map (see `src/store.rs` for the layout):

```
0x700000  PID slot   0x701000  ID slot   0x702000  blink slot
┌────────┐           ┌────────┐          ┌────────┐
│ PIDC + │           │ IDT1 + │          │ LBLK + │
│ target │           │ key    │          │ freq   │
└────────┘           └────────┘          └────────┘
0x703000  wifi slot (feature)
┌────────┐
│ WFI2   │  ssid · psk · udp · tcp
└────────┘
```

## Key points

- **Two writers, one handle.** The loop saves PID config; the Blink
  app's task saves frequency — hence the mutex, not a plain static.
- **Short and synchronous.** Every flash op runs to completion inside
  the critical section; nothing blocks across a wait.
- **One sector per thing, no wear levelling.** A config write is a
  user event; a looping writer is a bug this design doesn't hide.
