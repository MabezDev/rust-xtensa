use rustc_hir::def_id::DefId;
use rustc_middle::mir::visit::Visitor;
use rustc_middle::mir::*;
use rustc_middle::ty::{self, subst::GenericArgKind, PredicateAtom, Ty, TyCtxt, TyS};
use rustc_session::lint::builtin::FUNCTION_ITEM_REFERENCES;
use rustc_span::{symbol::sym, Span};
use rustc_target::spec::abi::Abi;

use crate::transform::MirPass;

pub struct FunctionItemReferences;

impl<'tcx> MirPass<'tcx> for FunctionItemReferences {
    fn run_pass(&self, tcx: TyCtxt<'tcx>, body: &mut Body<'tcx>) {
        let mut checker = FunctionItemRefChecker { tcx, body };
        checker.visit_body(&body);
    }
}

struct FunctionItemRefChecker<'a, 'tcx> {
    tcx: TyCtxt<'tcx>,
    body: &'a Body<'tcx>,
}

impl<'a, 'tcx> Visitor<'tcx> for FunctionItemRefChecker<'a, 'tcx> {
    fn visit_terminator(&mut self, terminator: &Terminator<'tcx>, location: Location) {
        if let TerminatorKind::Call {
            func,
            args,
            destination: _,
            cleanup: _,
            from_hir_call: _,
            fn_span: _,
        } = &terminator.kind
        {
            let func_ty = func.ty(self.body, self.tcx);
            if let ty::FnDef(def_id, substs_ref) = *func_ty.kind() {
                //check arguments for `std::mem::transmute`
                if self.tcx.is_diagnostic_item(sym::transmute, def_id) {
                    let arg_ty = args[0].ty(self.body, self.tcx);
                    for generic_inner_ty in arg_ty.walk() {
                        if let GenericArgKind::Type(inner_ty) = generic_inner_ty.unpack() {
                            if let Some(fn_id) = FunctionItemRefChecker::is_fn_ref(inner_ty) {
                                let ident = self.tcx.item_name(fn_id).to_ident_string();
                                let source_info = *self.body.source_info(location);
                                let span = self.nth_arg_span(&args, 0);
                                self.emit_lint(ident, fn_id, source_info, span);
                            }
                        }
                    }
                } else {
                    //check arguments for any function with `std::fmt::Pointer` as a bound trait
                    let param_env = self.tcx.param_env(def_id);
                    let bounds = param_env.caller_bounds();
                    for bound in bounds {
                        if let Some(bound_ty) = self.is_pointer_trait(&bound.skip_binders()) {
                            let arg_defs = self.tcx.fn_sig(def_id).skip_binder().inputs();
                            for (arg_num, arg_def) in arg_defs.iter().enumerate() {
                                for generic_inner_ty in arg_def.walk() {
                                    if let GenericArgKind::Type(inner_ty) =
                                        generic_inner_ty.unpack()
                                    {
                                        //if any type reachable from the argument types in the fn sig matches the type bound by `Pointer`
                                        if TyS::same_type(inner_ty, bound_ty) {
                                            //check if this type is a function reference in the function call
                                            let norm_ty =
                                                self.tcx.subst_and_normalize_erasing_regions(
                                                    substs_ref, param_env, &inner_ty,
                                                );
                                            if let Some(fn_id) =
                                                FunctionItemRefChecker::is_fn_ref(norm_ty)
                                            {
                                                let ident =
                                                    self.tcx.item_name(fn_id).to_ident_string();
                                                let source_info = *self.body.source_info(location);
                                                let span = self.nth_arg_span(&args, arg_num);
                                                self.emit_lint(ident, fn_id, source_info, span);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        self.super_terminator(terminator, location);
    }
    //check for `std::fmt::Pointer::<T>::fmt` where T is a function reference
    //this is used in formatting macros, but doesn't rely on the specific expansion
    fn visit_operand(&mut self, operand: &Operand<'tcx>, location: Location) {
        let op_ty = operand.ty(self.body, self.tcx);
        if let ty::FnDef(def_id, substs_ref) = *op_ty.kind() {
            if self.tcx.is_diagnostic_item(sym::pointer_trait_fmt, def_id) {
                let param_ty = substs_ref.type_at(0);
                if let Some(fn_id) = FunctionItemRefChecker::is_fn_ref(param_ty) {
                    let source_info = *self.body.source_info(location);
                    let callsite_ctxt = source_info.span.source_callsite().ctxt();
                    let span = source_info.span.with_ctxt(callsite_ctxt);
                    let ident = self.tcx.item_name(fn_id).to_ident_string();
                    self.emit_lint(ident, fn_id, source_info, span);
                }
            }
        }
        self.super_operand(operand, location);
    }
}

impl<'a, 'tcx> FunctionItemRefChecker<'a, 'tcx> {
    //return the bound parameter type if the trait is `std::fmt::Pointer`
    fn is_pointer_trait(&self, bound: &PredicateAtom<'tcx>) -> Option<Ty<'tcx>> {
        if let ty::PredicateAtom::Trait(predicate, _) = bound {
            if self.tcx.is_diagnostic_item(sym::pointer_trait, predicate.def_id()) {
                Some(predicate.trait_ref.self_ty())
            } else {
                None
            }
        } else {
            None
        }
    }
    fn is_fn_ref(ty: Ty<'tcx>) -> Option<DefId> {
        let referent_ty = match ty.kind() {
            ty::Ref(_, referent_ty, _) => Some(referent_ty),
            ty::RawPtr(ty_and_mut) => Some(&ty_and_mut.ty),
            _ => None,
        };
        referent_ty
            .map(
                |ref_ty| {
                    if let ty::FnDef(def_id, _) = *ref_ty.kind() { Some(def_id) } else { None }
                },
            )
            .unwrap_or(None)
    }
    fn nth_arg_span(&self, args: &Vec<Operand<'tcx>>, n: usize) -> Span {
        match &args[n] {
            Operand::Copy(place) | Operand::Move(place) => {
                self.body.local_decls[place.local].source_info.span
            }
            Operand::Constant(constant) => constant.span,
        }
    }
    fn emit_lint(&self, ident: String, fn_id: DefId, source_info: SourceInfo, span: Span) {
        let lint_root = self.body.source_scopes[source_info.scope]
            .local_data
            .as_ref()
            .assert_crate_local()
            .lint_root;
        let fn_sig = self.tcx.fn_sig(fn_id);
        let unsafety = fn_sig.unsafety().prefix_str();
        let abi = match fn_sig.abi() {
            Abi::Rust => String::from(""),
            other_abi => {
                let mut s = String::from("extern \"");
                s.push_str(other_abi.name());
                s.push_str("\" ");
                s
            }
        };
        let num_args = fn_sig.inputs().map_bound(|inputs| inputs.len()).skip_binder();
        let variadic = if fn_sig.c_variadic() { ", ..." } else { "" };
        let ret = if fn_sig.output().skip_binder().is_unit() { "" } else { " -> _" };
        self.tcx.struct_span_lint_hir(FUNCTION_ITEM_REFERENCES, lint_root, span, |lint| {
            lint.build(&format!(
                "cast `{}` with `as {}{}fn({}{}){}` to obtain a function pointer",
                ident,
                unsafety,
                abi,
                vec!["_"; num_args].join(", "),
                variadic,
                ret,
            ))
            .emit();
        });
    }
}
