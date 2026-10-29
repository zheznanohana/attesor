# AOT checkpoint reliability

The cache is a performance aid; incompatible translations are discarded and
rebuilt. These fixes address persistence and correctness, without claiming an
application startup speedup from cache-file existence alone.

## Changes

- Checkpoint workers drain partial batches from all guest threads, including
  threads that are sleeping. Previously only full 256-block batches reached the
  shared profile; a checkpoint's own thread-local batch was empty. Thread exit
  publishes its remaining tail as well.
- An incompatible AOT file is treated as missing for rebuild decisions. A larger
  file from an older generator or host-feature configuration cannot suppress a
  current smaller replacement. Header dimensions and block offsets are checked
  before allocating/registering cached code.
- The version-5 cache header records the exact profile fingerprint used by its
  builder. A profile published just before a killed builder no longer suppresses
  future work merely because an older compatible AOT file still exists. An older
  builder cannot replace a file already covering the latest on-disk profile.
- Cache compatibility includes host AES, PCLMULQDQ and SHA features, in addition
  to commpage/FMA/F16C, plus JIT ON/OFF/DISABLE code-generation options. A build's
  Mach-O UUID remains part of the generator identity.
- Guest fork no longer calls the process-exit save routine: that permanently
  disabled the parent's checkpoints and left the child with no checkpoint worker.
  The child restarts a worker after releasing the inherited locks. Background
  exit builders use the same AOT fork-lock protocol.
- AOT file temporary names distinguish overlapping builders within one process,
  not just different process IDs.

## Locking

The registry holds weak references to per-thread batches. Recording takes only
its thread's batch lock, then releases it before publishing to `SEEN`. The global
merge still occurs once per 256 recorded blocks. Hot cached execution does not
record a new block on every iteration. Cold-start overhead needs measurement.

Fork's lock order is registry, batches, `IMAGES`, `SEEN`, file-lock guard. Profile
updates release memory locks before acquiring file locks. Both fork branches
release all guards before guest execution or thread creation. File guards prevent
fork from inheriting another thread's open advisory-lock descriptor; translation
itself runs outside that guard. This protocol covers AOT locks, not every global
lock elsewhere in the emulator.

## Reproduction

From a release build in any checkout with the installed arm64 sysroot:

```sh
cargo test -p attesor aot::tests -- --test-threads=1
cargo build --release
python3 tests/attesor/aot-checkpoint.py --attesor target/release/attesor
```

The unit tests verify an idle thread's partial batch, an exited thread's tail,
rejection of larger incompatible headers and malformed dimensions. The process
test compiles private ARM guests and proves:

1. A live checkpoint writes a nonempty main-image translation, SIGKILL bypasses
   exit saving, and the next process actually loads the translation.
2. A changed JIT code-generation switch rejects the default translation.
3. A poisoned generator ID is repaired even with an existing profile, then loaded.
4. An older coverage fingerprint retries even without new profile blocks.
5. A four-thread guest forks and both parent and child complete checkpoints.
6. Killing that private process group leaves a cache that the next process loads.

All test artifacts and cache files use a private temporary directory, and only
processes started by the test are killed. This does not prove that a full Electron
application has a startup speedup. Background prewarming of whole applications,
and realistic app timing remain
follow-up work.
