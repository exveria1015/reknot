// Resident Windows dependencies and emulated TLS for Rust COFF generations.
#pragma once
#include "llvm/ADT/StringRef.h"
#include "llvm/Support/Error.h"
#include <cstddef>
#include <cstdint>
#include <memory>
#include <vector>

class RustRuntime {
  std::vector<void *> libraries;
  bool allow_emulated_tls = true;

public:
  static llvm::Expected<std::unique_ptr<RustRuntime>>
  load(const wchar_t *path, const wchar_t *support);
  void *find(llvm::StringRef name) const;
  static bool executable(void *address);
};

// Reclaim backing storage only after a real thread handle is signaled. This is
// later than every FLS/TLS destructor, independent of DLL notification order.
size_t collect_finished_tls_threads();
#ifdef REKNOT_JIT_TESTING
size_t retained_tls_threads();
size_t retained_tls_bytes();
#endif
