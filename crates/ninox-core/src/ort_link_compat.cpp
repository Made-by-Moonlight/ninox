// Companion to the `ort_link_compat` module in embeddings.rs — see there for
// the full story on why the prebuilt ONNX Runtime static libraries need
// compatibility help on distros with an older toolchain (e.g. Ubuntu 22.04).
//
// This translation unit fixes the *iostream initialization* half of that
// story. GCC 13.3 moved construction of std::cout/cerr/... out of user
// objects (the per-TU `__ioinit` static) and into libstdc++ itself
// (`_ZSt21ios_base_library_initv`). Objects compiled with the new scheme —
// like pyke's prebuilt ONNX Runtime — carry no iostream init of their own,
// so when they are linked against an older libstdc++ that does not
// self-initialize, std::cout is never constructed and the first write to it
// (onnxruntime logs from a static constructor via CPUIDInfo::LogEarlyWarning,
// and from its runtime logging paths) segfaults in std::ostream::sentry.
//
// Because this file is compiled by the *system* compiler against the
// *system* libstdc++ headers, it uses whichever init mechanism that
// libstdc++ actually implements: on an old toolchain the explicit
// ios_base::Init object below constructs the standard streams; on a new
// toolchain it is a no-op re-init. init_priority(101) (the lowest priority
// available to user code; default-priority constructors run after all
// numbered ones) makes it run before ONNX Runtime's static constructors.
#include <iostream>

namespace {
__attribute__((init_priority(101))) std::ios_base::Init ninox_force_iostream_init;
} // namespace

// The linker only pulls an archive member that satisfies an undefined
// symbol; a member containing nothing but a static initializer would be
// silently dropped. embeddings.rs references this anchor to force the
// member — and with it the initializer above — into the final link.
extern "C" void ninox_ort_link_compat_anchor() {}
