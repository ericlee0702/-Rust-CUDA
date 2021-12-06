//! Final steps in codegen, coalescing modules and feeding them to libnvvm.

use crate::builder::unnamed;
use crate::llvm::*;
use crate::lto::ThinBuffer;
use find_cuda_helper::find_cuda_root;
use nvvm::*;
use rustc_codegen_ssa::traits::ThinBufferMethods;
use rustc_session::Session;
use std::ffi::OsStr;
use std::fmt::Display;
use std::fs;
use std::marker::PhantomData;
use std::path::Path;
use tracing::debug;

// see libintrinsics.ll on what this is.
const LIBINTRINSICS: &[u8] = include_bytes!("../libintrinsics.bc");

pub enum CodegenErr {
    Nvvm(NvvmError),
    Io(std::io::Error),
}

impl From<std::io::Error> for CodegenErr {
    fn from(v: std::io::Error) -> Self {
        Self::Io(v)
    }
}

impl From<NvvmError> for CodegenErr {
    fn from(v: NvvmError) -> Self {
        Self::Nvvm(v)
    }
}

impl Display for CodegenErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Nvvm(err) => std::fmt::Display::fmt(&err, f),
            Self::Io(err) => std::fmt::Display::fmt(&err, f),
        }
    }
}

/// Take a list of bitcode module bytes and their names and codegen it
/// into ptx bytes. The final PTX *should* be utf8, but just to be on the safe side
/// it returns a vector of bytes.
///
/// Note that this will implicitly try to find libdevice and add it, so don't do that
/// step before this. It will fatal error if it cannot find it.
pub fn codegen_bitcode_modules(
    opts: &[NvvmOption],
    sess: &Session,
    modules: Vec<Vec<u8>>,
    llcx: &Context,
) -> Result<Vec<u8>, CodegenErr> {
    debug!("Codegenning bitcode to PTX");

    // make sure the nvvm version is high enough so users don't get confusing compilation errors.
    let (major, minor) = nvvm::ir_version();

    if minor < 6 || major < 1 {
        sess.fatal("rustc_codegen_nvvm requires at least libnvvm 1.6 (CUDA 11.2)");
    }

    // first, create the nvvm program we will add modules to.
    let prog = NvvmProgram::new()?;

    let module = merge_llvm_modules(modules, llcx);
    unsafe {
        internalize_pass(module, llcx);
        dce_pass(module);
    }
    let buf = ThinBuffer::new(module);

    prog.add_module(buf.data(), "merged".to_string())?;

    let libdevice = if let Some(bc) = find_libdevice() {
        bc
    } else {
        // i would put a more helpful error here, but to actually use the codegen
        // it needs to find libnvvm before this, and libdevice is in the nvvm directory
        // so if it can find libnvvm there is almost no way it can't find libdevice.
        sess.fatal("Could not find the libdevice library (libdevice.10.bc) in the CUDA directory")
    };

    prog.add_lazy_module(&libdevice, "libdevice".to_string())?;
    prog.add_lazy_module(LIBINTRINSICS, "libintrinsics".to_string())?;

    // for now, while the codegen is young, we always run verification on the program.
    // This is to make debugging much easier, libnvvm tends to infinitely loop or segfault on invalid programs
    // which makes debugging extremely hard. This way, if a malformed program is created, it is caught before
    // giving it to libnvvm. Then to debug codegen failures, we can just ask the user to provide the corresponding llvm ir
    // file with --emit=llvm-ir

    let verification_res = prog.verify();
    if verification_res.is_err() {
        let log = prog.compiler_log().unwrap().unwrap_or_default();
        let footer = "If you plan to submit a bug report please re-run the codegen with `RUSTFLAGS=\"--emit=llvm-ir\" and include the .ll file corresponding to the .o file mentioned in the log";
        panic!(
            "Malformed NVVM IR program rejected by libnvvm, dumping verifier log:\n\n{}\n\n{}",
            log, footer
        );
    }

    let res = match prog.compile(opts) {
        Ok(b) => b,
        Err(_) => {
            // this should never happen, if it does, something went really bad or its a bug on libnvvm's end
            panic!("libnvvm returned an error that was not previously caught by the verifier");
        }
    };

    Ok(res)
}

/// Find the libdevice bitcode library which contains math intrinsics and is
/// linked when building the nvvm program.
pub fn find_libdevice() -> Option<Vec<u8>> {
    if let Some(base_path) = find_cuda_root() {
        let libdevice_file = fs::read_dir(Path::new(&base_path).join("nvvm").join("libdevice"))
            .ok()?
            .filter_map(Result::ok)
            .find(|f| f.path().extension() == Some(OsStr::new("bc")))?
            .path();

        fs::read(libdevice_file).ok()
    } else {
        None
    }
}

// Merging and DCE (dead code elimination) logic. Inspired a lot by rust-ptx-linker.
//
// This works in a couple of steps starting from the bitcode of every single module (crate), then:
// - Merge all of the modules into a single large module, basically fat LTO. In the future we could probably lazily-load only
// the things we need using dependency graphs, like we used to do for libnvvm.
// - Iterate over every function in the module and:
//      - If it is not a kernel and it is not a declaration (i.e. an extern fn) then mark its linkage as internal and its visiblity as default
// - Iterate over every global in the module and:
//      - Same as functions, if it is not an external declaration, mark it as internal.
// - run LLVM's global DCE pass, this will remove any functions and globals that are not directly or indirectly used by kernels.

fn merge_llvm_modules(modules: Vec<Vec<u8>>, llcx: &Context) -> &Module {
    let module = unsafe { crate::create_module(llcx, "merged_modules") };
    for merged_module in modules {
        unsafe {
            let tmp = LLVMRustParseBitcodeForLTO(
                llcx,
                merged_module.as_ptr(),
                merged_module.len(),
                unnamed(),
            )
            .expect("Failed to parse module bitcode");
            LLVMLinkModules2(module, tmp);
        }
    }
    module
}

struct FunctionIter<'a, 'll> {
    module: PhantomData<&'a &'ll Module>,
    next: Option<&'ll Value>,
}

struct GlobalIter<'a, 'll> {
    module: PhantomData<&'a &'ll Module>,
    next: Option<&'ll Value>,
}

impl<'a, 'll> FunctionIter<'a, 'll> {
    pub fn new(module: &'a &'ll Module) -> Self {
        FunctionIter {
            module: PhantomData::default(),
            next: unsafe { LLVMGetFirstFunction(*module) },
        }
    }
}

impl<'a, 'll> Iterator for FunctionIter<'a, 'll> {
    type Item = &'ll Value;

    fn next(&mut self) -> Option<&'ll Value> {
        let next = self.next;

        self.next = match next {
            Some(next) => unsafe { LLVMGetNextFunction(&*next) },
            None => None,
        };

        next
    }
}

impl<'a, 'll> GlobalIter<'a, 'll> {
    pub fn new(module: &'a &'ll Module) -> Self {
        GlobalIter {
            module: PhantomData::default(),
            next: unsafe { LLVMGetFirstGlobal(*module) },
        }
    }
}

impl<'a, 'll> Iterator for GlobalIter<'a, 'll> {
    type Item = &'ll Value;

    fn next(&mut self) -> Option<&'ll Value> {
        let next = self.next;

        self.next = match next {
            Some(next) => unsafe { LLVMGetNextGlobal(&*next) },
            None => None,
        };

        next
    }
}

unsafe fn internalize_pass(module: &Module, cx: &Context) {
    // collect the values of all the declared kernels
    let num_operands =
        LLVMGetNamedMetadataNumOperands(module, "nvvm.annotations\0".as_ptr().cast()) as usize;
    let mut operands = Vec::with_capacity(num_operands);
    LLVMGetNamedMetadataOperands(
        module,
        "nvvm.annotations\0".as_ptr().cast(),
        operands.as_mut_ptr(),
    );
    operands.set_len(num_operands);
    let mut kernels = Vec::with_capacity(num_operands);
    let kernel_str = LLVMMDStringInContext(cx, "kernel".as_ptr().cast(), 6);

    for mdnode in operands {
        let num_operands = LLVMGetMDNodeNumOperands(mdnode) as usize;
        let mut operands = Vec::with_capacity(num_operands);
        LLVMGetMDNodeOperands(mdnode, operands.as_mut_ptr());
        operands.set_len(num_operands);

        if operands.get(1) == Some(&kernel_str) {
            kernels.push(operands[0]);
        }
    }

    // see what functions are marked as externally visible by the user.
    let num_operands =
        LLVMGetNamedMetadataNumOperands(module, "cg_nvvm_used\0".as_ptr().cast()) as usize;
    let mut operands = Vec::with_capacity(num_operands);
    LLVMGetNamedMetadataOperands(
        module,
        "cg_nvvm_used\0".as_ptr().cast(),
        operands.as_mut_ptr(),
    );
    operands.set_len(num_operands);
    let mut used_funcs = Vec::with_capacity(num_operands);

    for mdnode in operands {
        let num_operands = LLVMGetMDNodeNumOperands(mdnode) as usize;
        let mut operands = Vec::with_capacity(num_operands);
        LLVMGetMDNodeOperands(mdnode, operands.as_mut_ptr());
        operands.set_len(num_operands);

        used_funcs.push(operands[0]);
    }

    let iter = FunctionIter::new(&module);
    for func in iter {
        let is_kernel = kernels.contains(&func);
        let is_decl = LLVMIsDeclaration(func) == True;
        let is_used = used_funcs.contains(&func);

        if !is_decl && !is_kernel {
            LLVMRustSetLinkage(func, Linkage::InternalLinkage);
            LLVMRustSetVisibility(func, Visibility::Default);
        }

        // explicitly set it to external just in case the codegen set them to internal for some reason
        if is_used {
            LLVMRustSetLinkage(func, Linkage::ExternalLinkage);
            LLVMRustSetVisibility(func, Visibility::Default);
        }
    }

    let iter = GlobalIter::new(&module);
    for func in iter {
        let is_decl = LLVMIsDeclaration(func) == True;

        if !is_decl {
            LLVMRustSetLinkage(func, Linkage::InternalLinkage);
            LLVMRustSetVisibility(func, Visibility::Default);
        }
    }
}

unsafe fn dce_pass(module: &Module) {
    let pass_manager = LLVMCreatePassManager();

    LLVMAddGlobalDCEPass(pass_manager);

    LLVMRunPassManager(pass_manager, module);
    LLVMDisposePassManager(pass_manager);
}
