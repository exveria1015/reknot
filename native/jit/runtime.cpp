#include "runtime.h"
#include <algorithm>
#include <atomic>
#include <cstdlib>
#include <cstring>
#include <limits>
#include <malloc.h>
#include <new>
#include <windows.h>

// MSVC emits this RTTI vftable into each linked image instead of exporting it
// from vcruntime140.dll. Use the bridge's real CRT definition, not a fake
// table.
extern "C" const char reknot_type_info_vftable;
#pragma comment(linker,                                                        \
                "/alternatename:reknot_type_info_vftable=??_7type_info@@6B@")

namespace {
// LLVM's __emutls_control ABI on Windows x64. The descriptor and its
// initializer belong to a process-resident JIT generation. We key by descriptor
// identity; its optional index cache is deliberately left untouched.
struct TlsControl {
  uintptr_t size, alignment, index;
  const void *initial;
};
struct TlsThread {
  HANDLE thread;
  struct Value {
    const TlsControl *control;
    void *address;
    Value *next;
  };
  Value *values = nullptr;
  std::atomic<size_t> bytes{0};
  TlsThread *next = nullptr;
};
INIT_ONCE tls_once = INIT_ONCE_STATIC_INIT;
DWORD tls_key = TLS_OUT_OF_INDEXES;
SRWLOCK tls_lock = SRWLOCK_INIT;
TlsThread *tls_threads = nullptr;

BOOL CALLBACK initialize_tls(PINIT_ONCE, PVOID, PVOID *) {
  tls_key = TlsAlloc();
  return tls_key != TLS_OUT_OF_INDEXES;
}

[[noreturn]] void tls_failure() {
  // Like Rust's allocation failure, a failed TLS allocation cannot return a
  // null reference. Do not unwind across the compiler's nounwind helper call.
  std::abort();
}

extern "C" void *emutls_get_address(TlsControl *control) noexcept {
  // A compiler-inserted TLS access must not overwrite a Windows API error that
  // surrounding Rust code is about to inspect (TlsGetValue clears it).
  const DWORD last_error = GetLastError();
  if (!InitOnceExecuteOnce(&tls_once, initialize_tls, nullptr, nullptr))
    tls_failure();
  auto *thread = static_cast<TlsThread *>(TlsGetValue(tls_key));
  if (!thread) {
    collect_finished_tls_threads();
    thread = new (std::nothrow) TlsThread{};
    if (!thread ||
        !DuplicateHandle(GetCurrentProcess(), GetCurrentThread(),
                         GetCurrentProcess(), &thread->thread, SYNCHRONIZE,
                         FALSE, 0) ||
        !TlsSetValue(tls_key, thread))
      tls_failure();
    AcquireSRWLockExclusive(&tls_lock);
    thread->next = tls_threads;
    tls_threads = thread;
    ReleaseSRWLockExclusive(&tls_lock);
  }
  for (auto *value = thread->values; value; value = value->next)
    if (value->control == control) {
      SetLastError(last_error);
      return value->address;
    }
  auto align = control->alignment;
  auto size = control->size;
  if (!align || (align & (align - 1)) ||
      size > std::numeric_limits<size_t>::max() - align)
    tls_failure();
  void *value = _aligned_malloc(std::max(size, size_t(1)),
                                std::max(align, sizeof(void *)));
  if (!value)
    tls_failure();
  if (control->initial)
    std::memcpy(value, control->initial, size);
  else
    std::memset(value, 0, size);
  auto *node =
      new (std::nothrow) TlsThread::Value{control, value, thread->values};
  if (!node)
    tls_failure();
  thread->values = node;
  thread->bytes.fetch_add(size, std::memory_order_relaxed);
  SetLastError(last_error);
  return value;
}

llvm::Error load_error(const char *what) {
  return llvm::make_error<llvm::StringError>(
      std::string(what) + " (Windows error " + std::to_string(GetLastError()) +
          ")",
      llvm::inconvertibleErrorCode());
}
} // namespace

llvm::Expected<std::unique_ptr<RustRuntime>>
RustRuntime::load(const wchar_t *path, const wchar_t *support) {
  auto runtime = std::make_unique<RustRuntime>();
  runtime->allow_emulated_tls = support == nullptr;
  // The caller supplies an absolute path to the exact rustc's std DLL. All
  // successfully loaded dependencies stay resident, including on later errors.
  auto std = LoadLibraryExW(path, nullptr,
                            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR |
                                LOAD_LIBRARY_SEARCH_SYSTEM32);
  if (!std)
    return load_error("loading Rust std DLL failed");
  runtime->libraries.push_back(std);
  if (support) {
    // std is already resident. Load support with normal OS initialization and
    // a restricted dependency search; never search the working directory/PATH.
    auto library = LoadLibraryExW(support, nullptr,
                                 LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR |
                                     LOAD_LIBRARY_SEARCH_SYSTEM32);
    if (!library)
      return load_error("loading resident support DLL failed");
    runtime->libraries.push_back(library);
  }
  for (auto name :
       {L"vcruntime140.dll", L"ucrtbase.dll", L"kernel32.dll", L"ntdll.dll",
        L"userenv.dll", L"ws2_32.dll", L"bcryptprimitives.dll",
        L"api-ms-win-core-synch-l1-2-0.dll"}) {
    auto library = LoadLibraryExW(name, nullptr, LOAD_LIBRARY_SEARCH_SYSTEM32);
    if (!library)
      return load_error("loading Windows runtime dependency failed");
    runtime->libraries.push_back(library);
  }
  return runtime;
}

void *RustRuntime::find(llvm::StringRef name) const {
  if (name == "??_7type_info@@6B@")
    return const_cast<char *>(&reknot_type_info_vftable);
  if (allow_emulated_tls && name == "__emutls_get_address")
    return reinterpret_cast<void *>(&emutls_get_address);
  auto terminated = name.str();
  for (auto library : libraries)
    if (auto address =
            GetProcAddress(static_cast<HMODULE>(library), terminated.c_str()))
      return reinterpret_cast<void *>(address);
  return nullptr;
}

bool RustRuntime::executable(void *address) {
  MEMORY_BASIC_INFORMATION info{};
  if (!VirtualQuery(address, &info, sizeof(info)))
    return false;
  return (info.Protect & (PAGE_EXECUTE | PAGE_EXECUTE_READ |
                          PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY)) !=
         0;
}

size_t collect_finished_tls_threads() {
  size_t count = 0;
  AcquireSRWLockExclusive(&tls_lock);
  auto **link = &tls_threads;
  while (auto *thread = *link) {
    if (WaitForSingleObject(thread->thread, 0) != WAIT_OBJECT_0) {
      link = &thread->next;
      continue;
    }
    // The owning thread is gone, including every Rust TLS destructor. No live
    // thread mutates this map and thread-ID reuse cannot alias this HANDLE.
    *link = thread->next;
    while (auto *value = thread->values) {
      thread->values = value->next;
      _aligned_free(value->address);
      delete value;
    }
    CloseHandle(thread->thread);
    delete thread;
    ++count;
  }
  ReleaseSRWLockExclusive(&tls_lock);
  return count;
}

#ifdef REKNOT_JIT_TESTING
size_t retained_tls_threads() {
  size_t count = 0;
  AcquireSRWLockShared(&tls_lock);
  for (auto *thread = tls_threads; thread; thread = thread->next)
    ++count;
  ReleaseSRWLockShared(&tls_lock);
  return count;
}
size_t retained_tls_bytes() {
  size_t bytes = 0;
  AcquireSRWLockShared(&tls_lock);
  for (auto *thread = tls_threads; thread; thread = thread->next)
    bytes += thread->bytes.load(std::memory_order_relaxed);
  ReleaseSRWLockShared(&tls_lock);
  return bytes;
}
#endif
