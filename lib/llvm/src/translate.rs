//! Translation from LLVM IR to Cretonne IL.

use cretonne;
use cretonne::ir;
use cretonne::isa::TargetIsa;
use cton_frontend;
use std::collections::hash_map;
use std::error::Error;
use std::str;
use std::ptr;
use std::ffi;
use llvm_sys::prelude::*;
use llvm_sys::core::*;
use llvm_sys::ir_reader::*;
use llvm_sys::target::*;
use llvm_sys::LLVMValueKind::*;
use libc;

use operations::{translate_function_params, translate_inst};
use context::{Context, EbbInfo, Variable};
use module::{Module, CompiledFunction, DataSymbol, Compilation};
use reloc_sink::RelocSink;
use types::translate_sig;

/// Translate from an llvm-sys C-style string to a Rust String.
pub fn translate_string(charstar: *const libc::c_char) -> Result<String, String> {
    Ok(
        unsafe { ffi::CStr::from_ptr(charstar) }
            .to_str()
            .map_err(|err| err.description().to_string())?
            .to_string(),
    )
}

/// Translate from an llvm-sys C-style string to an `ir::FunctionName`.
pub fn translate_symbol_name(charstar: *const libc::c_char) -> Result<ir::ExternalName, String> {
    Ok(ir::ExternalName::new(
        translate_string(charstar)?.as_bytes(),
    ))
}

/// Create an LLVM Context.
pub fn create_llvm_context() -> LLVMContextRef {
    unsafe { LLVMContextCreate() }
}

/// Read LLVM IR (bitcode or text) from a file and return the resulting module.
pub fn read_llvm(llvm_ctx: LLVMContextRef, path: &str) -> Result<LLVMModuleRef, String> {
    let mut msg = ptr::null_mut();
    let mut buf = ptr::null_mut();
    let c_str = ffi::CString::new(path).map_err(
        |err| err.description().to_string(),
    )?;
    let llvm_name = c_str.as_ptr();
    if unsafe { LLVMCreateMemoryBufferWithContentsOfFile(llvm_name, &mut buf, &mut msg) } != 0 {
        return Err(format!(
            "error creating LLVM memory buffer for {}: {}",
            path,
            translate_string(msg)?
        ));
    }
    let mut module = ptr::null_mut();
    if unsafe { LLVMParseIRInContext(llvm_ctx, buf, &mut module, &mut msg) } != 0 {
        Err(format!(
            "error parsing LLVM IR in {}: {}",
            path,
            translate_string(msg)?
        ))
    } else {
        Ok(module)
    }
}

/// Translate an LLVM module into Cretonne IL.
pub fn translate_module(
    llvm_mod: LLVMModuleRef,
    isa: Option<&TargetIsa>,
) -> Result<Module, String> {
    // TODO: Use a more sophisticated API rather than just stuffing all
    // the functions into a Vec.
    let mut result = Module::new();
    let dl = unsafe { LLVMGetModuleDataLayout(llvm_mod) };

    // Translate the Functions.
    let mut llvm_func = unsafe { LLVMGetFirstFunction(llvm_mod) };
    while !llvm_func.is_null() {
        if unsafe { LLVMIsDeclaration(llvm_func) } != 0 {
            let llvm_name = unsafe { LLVMGetValueName(llvm_func) };
            let external_name = translate_symbol_name(llvm_name)?;
            result.unique_imports.insert(external_name.clone());
            result.imports.push(external_name);
        } else {
            let func = translate_function(llvm_func, dl, isa)?;
            result.functions.push(func);
        }
        llvm_func = unsafe { LLVMGetNextFunction(llvm_func) };
    }

    // Translate the GlobalVariables.
    let mut llvm_global = unsafe { LLVMGetFirstGlobal(llvm_mod) };
    while !llvm_global.is_null() {
        if unsafe { LLVMIsDeclaration(llvm_global) } != 0 {
            let llvm_name = unsafe { LLVMGetValueName(llvm_global) };
            let external_name = translate_symbol_name(llvm_name)?;
            result.unique_imports.insert(external_name.clone());
            result.imports.push(external_name);
        } else {
            let (name, contents) = translate_global(llvm_global, dl)?;
            result.data_symbols.push(DataSymbol { name, contents });
        }
        llvm_global = unsafe { LLVMGetNextGlobal(llvm_global) };
    }

    // TODO: GlobalAliases, ifuncs, metadata, comdat groups, inline asm

    Ok(result)
}

/// Translate the GlobalVariable `llvm_global` to Cretonne IL.
pub fn translate_global(
    llvm_global: LLVMValueRef,
    dl: LLVMTargetDataRef,
) -> Result<(ir::ExternalName, Vec<u8>), String> {
    let llvm_name = unsafe { LLVMGetValueName(llvm_global) };
    let name = translate_symbol_name(llvm_name)?;

    let llvm_ty = unsafe { LLVMGetElementType(LLVMTypeOf(llvm_global)) };
    let size = unsafe { LLVMABISizeOfType(dl, llvm_ty) };

    let llvm_init = unsafe { LLVMGetInitializer(llvm_global) };
    let llvm_kind = unsafe { LLVMGetValueKind(llvm_init) };
    let mut contents = Vec::new();
    match llvm_kind {
        LLVMConstantIntValueKind => {
            let raw = unsafe { LLVMConstIntGetSExtValue(llvm_init) };
            let mut part = raw;
            for _ in 0..size {
                contents.push((part & 0xff) as u8);
                part >>= 8;
            }
        }
        LLVMConstantAggregateZeroValueKind => {
            for _ in 0..size {
                contents.push(0u8);
            }
        }
        _ => {
            panic!(
                "unimplemented constant initializer value kind: {:?}",
                llvm_kind
            )
        }
    }

    Ok((name, contents))
}

/// Translate the Function `llvm_func` to Cretonne IL.
pub fn translate_function(
    llvm_func: LLVMValueRef,
    dl: LLVMTargetDataRef,
    isa: Option<&TargetIsa>,
) -> Result<CompiledFunction, String> {
    // TODO: Reuse the context between separate invocations.
    let mut cton_ctx = cretonne::Context::new();
    let llvm_name = unsafe { LLVMGetValueName(llvm_func) };
    cton_ctx.func.name = translate_symbol_name(llvm_name)?;
    cton_ctx.func.signature =
        translate_sig(unsafe { LLVMGetElementType(LLVMTypeOf(llvm_func)) }, dl);

    {
        let mut il_builder = cton_frontend::ILBuilder::<Variable>::new();
        let mut ctx = Context::new(&mut cton_ctx.func, &mut il_builder, dl);

        // Make a pre-pass through the basic blocks to collect predecessor
        // information, which LLVM's C API doesn't expose directly.
        let mut llvm_bb = unsafe { LLVMGetFirstBasicBlock(llvm_func) };
        while !llvm_bb.is_null() {
            prepare_for_bb(llvm_bb, &mut ctx);
            llvm_bb = unsafe { LLVMGetNextBasicBlock(llvm_bb) };
        }

        // Translate the contents of each basic block.
        llvm_bb = unsafe { LLVMGetFirstBasicBlock(llvm_func) };
        while !llvm_bb.is_null() {
            translate_bb(llvm_func, llvm_bb, &mut ctx);
            llvm_bb = unsafe { LLVMGetNextBasicBlock(llvm_bb) };
        }
    }

    if let Some(isa) = isa {
        let code_size = cton_ctx.compile(isa).map_err(
            |err| err.description().to_string(),
        )?;
        let mut code_buf: Vec<u8> = Vec::with_capacity(code_size as usize);
        let mut reloc_sink = RelocSink::new();
        code_buf.resize(code_size as usize, 0);
        cton_ctx.emit_to_memory(code_buf.as_mut_ptr(), &mut reloc_sink, isa);

        Ok(CompiledFunction {
            il: cton_ctx.func,
            compilation: Some(Compilation {
                body: code_buf,
                relocs: reloc_sink,
            }),
        })
    } else {
        Ok(CompiledFunction {
            il: cton_ctx.func,
            compilation: None,
        })
    }
}

/// Since LLVM's C API doesn't expose predecessor accessors, we make a prepass
/// and collect the information we need from the successor accessors.
fn prepare_for_bb(llvm_bb: LLVMBasicBlockRef, ctx: &mut Context) {
    let term = unsafe { LLVMGetBasicBlockTerminator(llvm_bb) };
    let is_switch = !unsafe { LLVMIsASwitchInst(term) }.is_null();
    let num_succs = unsafe { LLVMGetNumSuccessors(term) };
    for i in 0..num_succs {
        let llvm_succ = unsafe { LLVMGetSuccessor(term, i) };
        {
            let info = ctx.ebb_info.entry(llvm_succ).or_insert_with(
                EbbInfo::default,
            );
            info.num_preds_left += 1;
        }
        // If the block is reachable by branch (and not fallthrough), or by
        // a switch non-default edge (which can't use fallthrough), we need
        // an Ebb entry for it.
        if (is_switch && i != 0) || llvm_succ != unsafe { LLVMGetNextBasicBlock(llvm_bb) } {
            ctx.ebb_map.insert(llvm_succ, ctx.builder.create_ebb());
        }
    }
}

/// Translate the contents of `llvm_bb` to Cretonne IL instructions.
fn translate_bb(llvm_func: LLVMValueRef, llvm_bb: LLVMBasicBlockRef, ctx: &mut Context) {
    // Set up the Ebb as needed.
    if ctx.ebb_info.get(&llvm_bb).is_none() {
        // Block has no predecessors.
        let entry_block = llvm_bb == unsafe { LLVMGetEntryBasicBlock(llvm_func) };
        let ebb = ctx.builder.create_ebb();
        ctx.builder.seal_block(ebb);
        ctx.builder.switch_to_block(ebb);
        if entry_block {
            // It's the entry block. Add the parameters.
            translate_function_params(llvm_func, ebb, ctx);
        }
    } else if let hash_map::Entry::Occupied(entry) = ctx.ebb_map.entry(llvm_bb) {
        // Block has predecessors and is branched to, so it starts a new Ebb.
        let ebb = *entry.get();
        ctx.builder.switch_to_block(ebb);
    }

    // Translate each regular instruction.
    let mut llvm_inst = unsafe { LLVMGetFirstInstruction(llvm_bb) };
    while !llvm_inst.is_null() {
        translate_inst(llvm_bb, llvm_inst, ctx);
        llvm_inst = unsafe { LLVMGetNextInstruction(llvm_inst) };
    }

    // Visit each CFG successor and seal blocks that have had all their
    // predecessors visited.
    let term = unsafe { LLVMGetBasicBlockTerminator(llvm_bb) };
    let num_succs = unsafe { LLVMGetNumSuccessors(term) };
    for i in 0..num_succs {
        let llvm_succ = unsafe { LLVMGetSuccessor(term, i) };
        let info = ctx.ebb_info.get_mut(&llvm_succ).unwrap();
        debug_assert!(info.num_preds_left > 0);
        info.num_preds_left -= 1;
        if info.num_preds_left == 0 {
            if let Some(ebb) = ctx.ebb_map.get(&llvm_succ) {
                ctx.builder.seal_block(*ebb);
            }
        }
    }
}
