// Experimental single-owner, keep-loaded COFF backend. No general platform,
// arbitrary CRT/TLS, cross-boundary exceptions or automatic reclamation claim.
#include "llvm/ExecutionEngine/JITLink/JITLink.h"
#include "llvm/ExecutionEngine/JITLink/JITLinkMemoryManager.h"
#include "llvm/ExecutionEngine/Orc/Core.h"
#include "llvm/ExecutionEngine/Orc/ObjectLinkingLayer.h"
#include "llvm/ExecutionEngine/Orc/SelfExecutorProcessControl.h"
#include "llvm/Object/COFF.h"
#include "llvm/Support/MemoryBuffer.h"
#include <algorithm>
#include <chrono>
#include <cstdint>
#include <cstring>
#include <limits>
#include <memory>
#include <string>
#include <unordered_map>
#include <vector>
#include <windows.h>

using namespace llvm;
using namespace llvm::orc;
using Clock = std::chrono::steady_clock;
static double ms(Clock::duration d) {
  return std::chrono::duration<double, std::milli>(d).count();
}

struct Metrics {
  double copy_ms, add_ms, materialize_ms, seh_register_ms;
  uint64_t linked_bytes, seh_entries;
};
struct Image {
  uint64_t base = 0, end = 0, bytes = 0;
  double seh_ms = 0;
  std::vector<RUNTIME_FUNCTION> functions;
  bool registered = false;
};
struct Engine {
  std::unique_ptr<ExecutionSession> session;
  std::unique_ptr<ObjectLinkingLayer> layer;
  std::unordered_map<JITDylib *, std::unique_ptr<Image>> images;
  JITDylib *last = nullptr;
  uint64_t sequence = 0;
};

static Error fail(const char *message) {
  return make_error<StringError>(message, inconvertibleErrorCode());
}
static void error_text(char *buffer, size_t length, std::string text) {
  if (!buffer || !length)
    return;
  auto n = std::min(length - 1, text.size());
  std::memcpy(buffer, text.data(), n);
  buffer[n] = 0;
}

class CoffPlugin : public ObjectLinkingLayer::Plugin {
  Engine &engine;

public:
  explicit CoffPlugin(Engine &e) : engine(e) {}
  void modifyPassConfig(MaterializationResponsibility &mr, jitlink::LinkGraph &,
                        jitlink::PassConfiguration &config) override {
    auto &image = engine.images.at(&mr.getTargetJITDylib());
    Image *record = image.get();
    // Provide the synthetic image base before external-symbol resolution. The
    // actual address is fixed after allocation, before COFF RVA lowering.
    config.PostPrunePasses.push_back([](jitlink::LinkGraph &g) -> Error {
      std::vector<jitlink::Symbol *> bases;
      for (auto *s : g.external_symbols())
        if (*s->getName() == "__ImageBase")
          bases.push_back(s);
      for (auto *s : bases)
        g.makeAbsolute(*s, ExecutorAddr(0));
      return Error::success();
    });
    config.PostAllocationPasses.push_back(
        [record](jitlink::LinkGraph &g) -> Error {
          uint64_t first = std::numeric_limits<uint64_t>::max(), last = 0;
          for (auto *b : g.blocks()) {
            first = std::min(first, b->getAddress().getValue());
            last = std::max(last, b->getAddress().getValue() + b->getSize());
            record->bytes += b->getSize();
          }
          if (first == std::numeric_limits<uint64_t>::max() ||
              last - first > UINT32_MAX)
            return fail("COFF object does not fit a 32-bit RVA range");
          record->base = first;
          record->end = last;
          for (auto *s : g.absolute_symbols())
            if (*s->getName() == "__ImageBase")
              s->getAddressable().setAddress(ExecutorAddr(first));
          return Error::success();
        });
    config.PostFixupPasses.push_back([record](jitlink::LinkGraph &g) -> Error {
      if (auto *section = g.findSectionByName(".pdata")) {
        for (auto *block : section->blocks()) {
          auto data = block->getContent();
          if (data.size() % sizeof(RUNTIME_FUNCTION))
            return fail("invalid .pdata length");
          for (size_t offset = 0; offset < data.size();
               offset += sizeof(RUNTIME_FUNCTION)) {
            RUNTIME_FUNCTION f;
            std::memcpy(&f, data.data() + offset, sizeof(f));
            if (f.BeginAddress >= f.EndAddress ||
                record->base + f.EndAddress > record->end)
              return fail("invalid .pdata code range");
            record->functions.push_back(f);
          }
        }
      }
      std::sort(record->functions.begin(), record->functions.end(),
                [](const auto &a, const auto &b) {
                  return a.BeginAddress < b.BeginAddress;
                });
      return Error::success();
    });
  }
  Error notifyEmitted(MaterializationResponsibility &mr) override {
    auto &record = *engine.images.at(&mr.getTargetJITDylib());
    if (record.functions.empty())
      return fail("fixture has no .pdata to register");
    auto start = Clock::now();
    if (!RtlAddFunctionTable(record.functions.data(),
                             static_cast<DWORD>(record.functions.size()),
                             record.base))
      return fail("RtlAddFunctionTable failed");
    record.seh_ms = ms(Clock::now() - start);
    record.registered = true;
    return Error::success();
  }
  Error notifyFailed(MaterializationResponsibility &mr) override {
    auto &record = *engine.images.at(&mr.getTargetJITDylib());
    // No callback is published until lookup succeeds. If materialization fails
    // after registration, remove the OS table before LLVM releases allocations.
    if (record.registered) {
      if (!RtlDeleteFunctionTable(record.functions.data()))
        report_fatal_error(
            "failed to unregister an unpublished JIT unwind table");
      record.registered = false;
    }
    return Error::success();
  }
  Error notifyRemovingResources(JITDylib &, ResourceKey) override {
    return fail("prototype keeps JIT images until process exit");
  }
  void notifyTransferringResources(JITDylib &, ResourceKey,
                                   ResourceKey) override {}
};

extern "C" __declspec(dllexport) uint32_t reknot_jit_bridge_abi() { return 1; }

extern "C" __declspec(dllexport) void *reknot_jit_create(char *error,
                                                         size_t capacity) {
  auto epc = SelfExecutorProcessControl::Create();
  if (!epc) {
    error_text(error, capacity, toString(epc.takeError()));
    return nullptr;
  }
  auto memory = jitlink::InProcessMemoryManager::Create();
  if (!memory) {
    error_text(error, capacity, toString(memory.takeError()));
    return nullptr;
  }
  // Deliberately process-lifetime: no destructor can free code still on a
  // stack.
  auto *engine = new Engine();
  engine->session = std::make_unique<ExecutionSession>(std::move(*epc));
  engine->layer = std::make_unique<ObjectLinkingLayer>(*engine->session,
                                                       std::move(*memory));
  engine->layer->addPlugin(std::make_unique<CoffPlugin>(*engine));
  return engine;
}

extern "C" __declspec(dllexport) int
reknot_jit_load(void *opaque, const uint8_t *bytes, size_t length,
                uint64_t *entry, Metrics *metrics, char *error,
                size_t capacity) {
  auto &engine = *static_cast<Engine *>(opaque);
  auto begin = Clock::now();
  auto buffer = MemoryBuffer::getMemBufferCopy(
      StringRef(reinterpret_cast<const char *>(bytes), length), "reknot-coff");
  auto parsed = object::ObjectFile::createObjectFile(buffer->getMemBufferRef());
  if (!parsed) {
    error_text(error, capacity, toString(parsed.takeError()));
    return 0;
  }
  auto *coff = dyn_cast<object::COFFObjectFile>(parsed->get());
  if (!coff || coff->getDOSHeader() || coff->getMachine() != 0x8664) {
    error_text(error, capacity,
               "expected an x86-64 COFF object, not a linked image");
    return 0;
  }
  for (const auto &section : coff->sections()) {
    auto name = section.getName();
    if (!name) {
      error_text(error, capacity, toString(name.takeError()));
      return 0;
    }
    if (name->starts_with(".tls") || name->starts_with(".CRT") ||
        name->starts_with(".ctors") || name->starts_with(".dtors")) {
      error_text(error, capacity,
                 "TLS and runtime startup/destructor sections are unsupported");
      return 0;
    }
  }
  parsed->reset();
  // No process-symbol generator: unsupported runtime imports fail closed.
  auto read = Clock::now();
  auto &jd = engine.session->createBareJITDylib(
      "generation-" + std::to_string(++engine.sequence));
  engine.last = &jd;
  engine.images.emplace(&jd, std::make_unique<Image>());
  auto tracker = jd.createResourceTracker();
  if (auto e = engine.layer->add(tracker, std::move(buffer))) {
    error_text(error, capacity, toString(std::move(e)));
    return 0;
  }
  auto added = Clock::now();
  auto result = engine.session->lookup({&jd}, "hot_handler_api");
  if (!result) {
    error_text(error, capacity, toString(result.takeError()));
    return 0;
  }
  auto linked = Clock::now();
  auto &image = *engine.images.at(&jd);
  *entry = result->getAddress().getValue();
  *metrics = {
      ms(read - begin),   ms(added - read),
      ms(linked - added), image.seh_ms,
      image.bytes,        static_cast<uint64_t>(image.functions.size())};
  return image.registered ? 1 : 0;
}

#ifdef REKNOT_JIT_TESTING
extern "C" __declspec(dllexport) int
reknot_jit_test_symbol(void *opaque, const char *symbol, uint64_t *entry,
                       char *error, size_t capacity) {
  auto &engine = *static_cast<Engine *>(opaque);
  if (!engine.last) {
    error_text(error, capacity, "no JIT image");
    return 0;
  }
  auto result = engine.session->lookup({engine.last}, symbol);
  if (!result) {
    error_text(error, capacity, toString(result.takeError()));
    return 0;
  }
  *entry = result->getAddress().getValue();
  return 1;
}

static thread_local DWORD64 probe_base = 0;
static thread_local unsigned probe_frames = 0;
static void stack_probe(uint64_t) {
  void *frames[64];
  const auto count = CaptureStackBackTrace(0, 64, frames, nullptr);
  for (USHORT i = 0; i < count; ++i) {
    DWORD64 base = 0;
    if (RtlLookupFunctionEntry(reinterpret_cast<DWORD64>(frames[i]), &base,
                               nullptr) &&
        base == probe_base)
      ++probe_frames;
  }
}
extern "C" __declspec(dllexport) unsigned
reknot_jit_test_stack(uint64_t entry) {
  probe_base = 0;
  probe_frames = 0;
  if (!RtlLookupFunctionEntry(entry, &probe_base, nullptr))
    return 0;
  auto call =
      reinterpret_cast<uint64_t (*)(uint64_t, void (*)(uint64_t))>(entry);
  call(7, stack_probe);
  return probe_frames;
}
#endif
