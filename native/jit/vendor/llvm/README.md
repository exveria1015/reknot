# Local LLVM experiment

These files are from `llvmorg-23.1.0` under `llvm/lib/ExecutionEngine/JITLink/`.
The LLVM Apache-2.0-with-exceptions license is preserved in `LICENSE.TXT`.

Source: https://github.com/llvm/llvm-project/tree/llvmorg-23.1.0/llvm/lib/ExecutionEngine/JITLink

Only `COFFLinkGraphBuilder.cpp` is modified. In `createDefinedSymbol`, a local
strong COMDAT leader at offset zero now resolves the pending section-symbol
alias and clears that pending request. Upstream handles that alias for external
COMDAT leaders but leaves it pending for these rustc local leaders. Relocations
from `.pdata` and CodeView sections can consequently fail to resolve the original
section-symbol index. The patch rejects other local COMDAT forms rather than
claiming support for them. This is a narrow experiment patch, not a general
upstream-ready COFF compatibility fix.

CMake builds this one source file into the bridge ahead of the SDK's static
archive. The installed LLVM SDK remains untouched. The bridge always includes
this patch; the upstream implementation does not resolve these local aliases.
The other headers and `COFFOptions.td` are unmodified upstream files. CMake uses
the SDK's llvm-tblgen and OptParser.td to generate COFFOptions.inc.
