//! The `Visitor` responsible for actually checking a `mir::Body` for invalid operations.

use rustc::hir::{HirId, def_id::DefId};
use rustc::middle::lang_items;
use rustc::mir::visit::{PlaceContext, Visitor, MutatingUseContext, NonMutatingUseContext};
use rustc::mir::*;
use rustc::traits::{self, TraitEngine};
use rustc::ty::cast::CastTy;
use rustc::ty::{self, TyCtxt};
use rustc_index::bit_set::BitSet;
use rustc_target::spec::abi::Abi;
use rustc_error_codes::*;
use syntax::symbol::sym;
use syntax_pos::Span;

use std::borrow::Cow;
use std::ops::Deref;

use crate::dataflow::{self as old_dataflow, generic as dataflow};
use self::old_dataflow::IndirectlyMutableLocals;
use super::ops::{self, NonConstOp};
use super::qualifs::{self, HasMutInterior, NeedsDrop};
use super::resolver::FlowSensitiveAnalysis;
use super::{ConstKind, Item, Qualif, is_lang_panic_fn};

pub type IndirectlyMutableResults<'mir, 'tcx> =
    old_dataflow::DataflowResultsCursor<'mir, 'tcx, IndirectlyMutableLocals<'mir, 'tcx>>;

struct QualifCursor<'a, 'mir, 'tcx, Q: Qualif> {
    cursor: dataflow::ResultsCursor<'mir, 'tcx, FlowSensitiveAnalysis<'a, 'mir, 'tcx, Q>>,
    in_any_value_of_ty: BitSet<Local>,
}

impl<Q: Qualif> QualifCursor<'a, 'mir, 'tcx, Q> {
    pub fn new(
        q: Q,
        item: &'a Item<'mir, 'tcx>,
        dead_unwinds: &BitSet<BasicBlock>,
    ) -> Self {
        let analysis = FlowSensitiveAnalysis::new(q, item);
        let results =
            dataflow::Engine::new(item.tcx, item.body, item.def_id, dead_unwinds, analysis)
                .iterate_to_fixpoint();
        let cursor = dataflow::ResultsCursor::new(item.body, results);

        let mut in_any_value_of_ty = BitSet::new_empty(item.body.local_decls.len());
        for (local, decl) in item.body.local_decls.iter_enumerated() {
            if Q::in_any_value_of_ty(item, decl.ty) {
                in_any_value_of_ty.insert(local);
            }
        }

        QualifCursor {
            cursor,
            in_any_value_of_ty,
        }
    }
}

pub struct Qualifs<'a, 'mir, 'tcx> {
    has_mut_interior: QualifCursor<'a, 'mir, 'tcx, HasMutInterior>,
    needs_drop: QualifCursor<'a, 'mir, 'tcx, NeedsDrop>,
    indirectly_mutable: IndirectlyMutableResults<'mir, 'tcx>,
}

impl Qualifs<'a, 'mir, 'tcx> {
    fn indirectly_mutable(&mut self, local: Local, location: Location) -> bool {
        self.indirectly_mutable.seek(location);
        self.indirectly_mutable.get().contains(local)
    }

    /// Returns `true` if `local` is `NeedsDrop` at the given `Location`.
    ///
    /// Only updates the cursor if absolutely necessary
    fn needs_drop_lazy_seek(&mut self, local: Local, location: Location) -> bool {
        if !self.needs_drop.in_any_value_of_ty.contains(local) {
            return false;
        }

        self.needs_drop.cursor.seek_before(location);
        self.needs_drop.cursor.get().contains(local)
            || self.indirectly_mutable(local, location)
    }

    /// Returns `true` if `local` is `HasMutInterior` at the given `Location`.
    ///
    /// Only updates the cursor if absolutely necessary.
    fn has_mut_interior_lazy_seek(&mut self, local: Local, location: Location) -> bool {
        if !self.has_mut_interior.in_any_value_of_ty.contains(local) {
            return false;
        }

        self.has_mut_interior.cursor.seek_before(location);
        self.has_mut_interior.cursor.get().contains(local)
            || self.indirectly_mutable(local, location)
    }

    /// Returns `true` if `local` is `HasMutInterior`, but requires the `has_mut_interior` and
    /// `indirectly_mutable` cursors to be updated beforehand.
    fn has_mut_interior_eager_seek(&self, local: Local) -> bool {
        if !self.has_mut_interior.in_any_value_of_ty.contains(local) {
            return false;
        }

        self.has_mut_interior.cursor.get().contains(local)
            || self.indirectly_mutable.get().contains(local)
    }

    fn in_return_place(&mut self, item: &Item<'_, 'tcx>) -> ConstQualifs {
        // Find the `Return` terminator if one exists.
        //
        // If no `Return` terminator exists, this MIR is divergent. Just return the conservative
        // qualifs for the return type.
        let return_block = item.body
            .basic_blocks()
            .iter_enumerated()
            .find(|(_, block)| {
                match block.terminator().kind {
                    TerminatorKind::Return => true,
                    _ => false,
                }
            })
            .map(|(bb, _)| bb);

        let return_block = match return_block {
            None => return qualifs::in_any_value_of_ty(item, item.body.return_ty()),
            Some(bb) => bb,
        };

        let return_loc = item.body.terminator_loc(return_block);

        ConstQualifs {
            needs_drop: self.needs_drop_lazy_seek(RETURN_PLACE, return_loc),
            has_mut_interior: self.has_mut_interior_lazy_seek(RETURN_PLACE, return_loc),
        }
    }
}

pub struct Validator<'a, 'mir, 'tcx> {
    item: &'a Item<'mir, 'tcx>,
    qualifs: Qualifs<'a, 'mir, 'tcx>,

    /// The span of the current statement.
    span: Span,
}

impl Deref for Validator<'_, 'mir, 'tcx> {
    type Target = Item<'mir, 'tcx>;

    fn deref(&self) -> &Self::Target {
        &self.item
    }
}

impl Validator<'a, 'mir, 'tcx> {
    pub fn new(
        item: &'a Item<'mir, 'tcx>,
    ) -> Self {
        let dead_unwinds = BitSet::new_empty(item.body.basic_blocks().len());

        let needs_drop = QualifCursor::new(
            NeedsDrop,
            item,
            &dead_unwinds,
        );

        let has_mut_interior = QualifCursor::new(
            HasMutInterior,
            item,
            &dead_unwinds,
        );

        let indirectly_mutable = old_dataflow::do_dataflow(
            item.tcx,
            item.body,
            item.def_id,
            &item.tcx.get_attrs(item.def_id),
            &dead_unwinds,
            old_dataflow::IndirectlyMutableLocals::new(item.tcx, item.body, item.param_env),
            |_, local| old_dataflow::DebugFormatted::new(&local),
        );

        let indirectly_mutable = old_dataflow::DataflowResultsCursor::new(
            indirectly_mutable,
            item.body,
        );

        let qualifs = Qualifs {
            needs_drop,
            has_mut_interior,
            indirectly_mutable,
        };

        Validator {
            span: item.body.span,
            item,
            qualifs,
        }
    }

    pub fn check_body(&mut self) {
        let Item { tcx, body, def_id, const_kind, ..  } = *self.item;

        let use_min_const_fn_checks =
            tcx.is_min_const_fn(def_id)
            && !tcx.sess.opts.debugging_opts.unleash_the_miri_inside_of_you;

        if use_min_const_fn_checks {
            // Enforce `min_const_fn` for stable `const fn`s.
            use crate::transform::qualify_min_const_fn::is_min_const_fn;
            if let Err((span, err)) = is_min_const_fn(tcx, def_id, body) {
                error_min_const_fn_violation(tcx, span, err);
                return;
            }
        }

        check_short_circuiting_in_const_local(self.item);

        if body.is_cfg_cyclic() {
            // We can't provide a good span for the error here, but this should be caught by the
            // HIR const-checker anyways.
            self.check_op_spanned(ops::Loop, body.span);
        }

        self.visit_body(body);

        // Ensure that the end result is `Sync` in a non-thread local `static`.
        let should_check_for_sync = const_kind == Some(ConstKind::Static)
            && !tcx.has_attr(def_id, sym::thread_local);

        if should_check_for_sync {
            let hir_id = tcx.hir().as_local_hir_id(def_id).unwrap();
            check_return_ty_is_sync(tcx, body, hir_id);
        }
    }

    pub fn qualifs_in_return_place(&mut self) -> ConstQualifs {
        self.qualifs.in_return_place(self.item)
    }

    /// Emits an error at the given `span` if an expression cannot be evaluated in the current
    /// context.
    pub fn check_op_spanned<O>(&mut self, op: O, span: Span)
    where
        O: NonConstOp
    {
        trace!("check_op: op={:?}", op);

        if op.is_allowed_in_item(self) {
            return;
        }

        // If an operation is supported in miri (and is not already controlled by a feature gate) it
        // can be turned on with `-Zunleash-the-miri-inside-of-you`.
        let is_unleashable = O::IS_SUPPORTED_IN_MIRI
            && O::feature_gate(self.tcx).is_none();

        if is_unleashable && self.tcx.sess.opts.debugging_opts.unleash_the_miri_inside_of_you {
            self.tcx.sess.span_warn(span, "skipping const checks");
            return;
        }

        op.emit_error(self, span);
    }

    /// Emits an error if an expression cannot be evaluated in the current context.
    pub fn check_op(&mut self, op: impl NonConstOp) {
        let span = self.span;
        self.check_op_spanned(op, span)
    }

    fn check_static(&mut self, def_id: DefId, span: Span) {
        let is_thread_local = self.tcx.has_attr(def_id, sym::thread_local);
        if is_thread_local {
            self.check_op_spanned(ops::ThreadLocalAccess, span)
        } else {
            self.check_op_spanned(ops::StaticAccess, span)
        }
    }
}

impl Visitor<'tcx> for Validator<'_, 'mir, 'tcx> {
    fn visit_basic_block_data(
        &mut self,
        bb: BasicBlock,
        block: &BasicBlockData<'tcx>,
    ) {
        trace!("visit_basic_block_data: bb={:?} is_cleanup={:?}", bb, block.is_cleanup);

        // Just as the old checker did, we skip const-checking basic blocks on the unwind path.
        // These blocks often drop locals that would otherwise be returned from the function.
        //
        // FIXME: This shouldn't be unsound since a panic at compile time will cause a compiler
        // error anyway, but maybe we should do more here?
        if block.is_cleanup {
            return;
        }

        self.super_basic_block_data(bb, block);
    }

    fn visit_rvalue(&mut self, rvalue: &Rvalue<'tcx>, location: Location) {
        trace!("visit_rvalue: rvalue={:?} location={:?}", rvalue, location);

        // Special-case reborrows to be more like a copy of a reference.
        if let Rvalue::Ref(_, kind, ref place) = *rvalue {
            if let Some(reborrowed_proj) = place_as_reborrow(self.tcx, self.body, place) {
                let ctx = match kind {
                    BorrowKind::Shared => PlaceContext::NonMutatingUse(
                        NonMutatingUseContext::SharedBorrow,
                    ),
                    BorrowKind::Shallow => PlaceContext::NonMutatingUse(
                        NonMutatingUseContext::ShallowBorrow,
                    ),
                    BorrowKind::Unique => PlaceContext::NonMutatingUse(
                        NonMutatingUseContext::UniqueBorrow,
                    ),
                    BorrowKind::Mut { .. } => PlaceContext::MutatingUse(
                        MutatingUseContext::Borrow,
                    ),
                };
                self.visit_place_base(&place.base, ctx, location);
                self.visit_projection(&place.base, reborrowed_proj, ctx, location);
                return;
            }
        }

        self.super_rvalue(rvalue, location);

        match *rvalue {
            Rvalue::Use(_) |
            Rvalue::Repeat(..) |
            Rvalue::UnaryOp(UnOp::Neg, _) |
            Rvalue::UnaryOp(UnOp::Not, _) |
            Rvalue::NullaryOp(NullOp::SizeOf, _) |
            Rvalue::CheckedBinaryOp(..) |
            Rvalue::Cast(CastKind::Pointer(_), ..) |
            Rvalue::Discriminant(..) |
            Rvalue::Len(_) |
            Rvalue::Aggregate(..) => {}

            | Rvalue::Ref(_, kind @ BorrowKind::Mut { .. }, ref place)
            | Rvalue::Ref(_, kind @ BorrowKind::Unique, ref place)
            => {
                let ty = place.ty(self.body, self.tcx).ty;
                let is_allowed = match ty.kind {
                    // Inside a `static mut`, `&mut [...]` is allowed.
                    ty::Array(..) | ty::Slice(_) if self.const_kind() == ConstKind::StaticMut
                        => true,

                    // FIXME(ecstaticmorse): We could allow `&mut []` inside a const context given
                    // that this is merely a ZST and it is already eligible for promotion.
                    // This may require an RFC?
                    /*
                    ty::Array(_, len) if len.try_eval_usize(cx.tcx, cx.param_env) == Some(0)
                        => true,
                    */

                    _ => false,
                };

                if !is_allowed {
                    self.check_op(ops::MutBorrow(kind));
                }
            }

            // At the moment, `PlaceBase::Static` is only used for promoted MIR.
            | Rvalue::Ref(_, BorrowKind::Shared, ref place)
            | Rvalue::Ref(_, BorrowKind::Shallow, ref place)
            if matches!(place.base, PlaceBase::Static(_))
            => bug!("Saw a promoted during const-checking, which must run before promotion"),

            | Rvalue::Ref(_, kind @ BorrowKind::Shared, ref place)
            | Rvalue::Ref(_, kind @ BorrowKind::Shallow, ref place)
            => {
                // FIXME: Change the `in_*` methods to take a `FnMut` so we don't have to manually
                // seek the cursors beforehand.
                self.qualifs.has_mut_interior.cursor.seek_before(location);
                self.qualifs.indirectly_mutable.seek(location);

                let borrowed_place_has_mut_interior = HasMutInterior::in_place(
                    &self.item,
                    &|local| self.qualifs.has_mut_interior_eager_seek(local),
                    place.as_ref(),
                );

                if borrowed_place_has_mut_interior {
                    self.check_op(ops::MutBorrow(kind));
                }
            }

            Rvalue::Cast(CastKind::Misc, ref operand, cast_ty) => {
                let operand_ty = operand.ty(self.body, self.tcx);
                let cast_in = CastTy::from_ty(operand_ty).expect("bad input type for cast");
                let cast_out = CastTy::from_ty(cast_ty).expect("bad output type for cast");

                if let (CastTy::Ptr(_), CastTy::Int(_))
                     | (CastTy::FnPtr,  CastTy::Int(_)) = (cast_in, cast_out) {
                    self.check_op(ops::RawPtrToIntCast);
                }
            }

            Rvalue::BinaryOp(op, ref lhs, _) => {
                if let ty::RawPtr(_) | ty::FnPtr(..) = lhs.ty(self.body, self.tcx).kind {
                    assert!(op == BinOp::Eq || op == BinOp::Ne ||
                            op == BinOp::Le || op == BinOp::Lt ||
                            op == BinOp::Ge || op == BinOp::Gt ||
                            op == BinOp::Offset);


                    self.check_op(ops::RawPtrComparison);
                }
            }

            Rvalue::NullaryOp(NullOp::Box, _) => {
                self.check_op(ops::HeapAllocation);
            }
        }
    }

    fn visit_place_base(
        &mut self,
        place_base: &PlaceBase<'tcx>,
        context: PlaceContext,
        location: Location,
    ) {
        trace!(
            "visit_place_base: place_base={:?} context={:?} location={:?}",
            place_base,
            context,
            location,
        );
        self.super_place_base(place_base, context, location);

        match place_base {
            PlaceBase::Local(_) => {}
            PlaceBase::Static(_) => {
                bug!("Promotion must be run after const validation");
            }
        }
    }

    fn visit_operand(
        &mut self,
        op: &Operand<'tcx>,
        location: Location,
    ) {
        self.super_operand(op, location);
        if let Operand::Constant(c) = op {
            if let Some(def_id) = c.check_static_ptr(self.tcx) {
                self.check_static(def_id, self.span);
            }
        }
    }

    fn visit_projection_elem(
        &mut self,
        place_base: &PlaceBase<'tcx>,
        proj_base: &[PlaceElem<'tcx>],
        elem: &PlaceElem<'tcx>,
        context: PlaceContext,
        location: Location,
    ) {
        trace!(
            "visit_projection_elem: place_base={:?} proj_base={:?} elem={:?} \
            context={:?} location={:?}",
            place_base,
            proj_base,
            elem,
            context,
            location,
        );

        self.super_projection_elem(place_base, proj_base, elem, context, location);

        match elem {
            ProjectionElem::Deref => {
                let base_ty = Place::ty_from(place_base, proj_base, self.body, self.tcx).ty;
                if let ty::RawPtr(_) = base_ty.kind {
                    if proj_base.is_empty() {
                        if let (PlaceBase::Local(local), []) = (place_base, proj_base) {
                            let decl = &self.body.local_decls[*local];
                            if let LocalInfo::StaticRef { def_id, .. } = decl.local_info {
                                let span = decl.source_info.span;
                                self.check_static(def_id, span);
                                return;
                            }
                        }
                    }
                    self.check_op(ops::RawPtrDeref);
                }

                if context.is_mutating_use() {
                    self.check_op(ops::MutDeref);
                }
            }

            ProjectionElem::ConstantIndex {..} |
            ProjectionElem::Subslice {..} |
            ProjectionElem::Field(..) |
            ProjectionElem::Index(_) => {
                let base_ty = Place::ty_from(place_base, proj_base, self.body, self.tcx).ty;
                match base_ty.ty_adt_def() {
                    Some(def) if def.is_union() => {
                        self.check_op(ops::UnionAccess);
                    }

                    _ => {}
                }
            }

            ProjectionElem::Downcast(..) => {
                self.check_op(ops::Downcast);
            }
        }
    }


    fn visit_source_info(&mut self, source_info: &SourceInfo) {
        trace!("visit_source_info: source_info={:?}", source_info);
        self.span = source_info.span;
    }

    fn visit_statement(&mut self, statement: &Statement<'tcx>, location: Location) {
        trace!("visit_statement: statement={:?} location={:?}", statement, location);

        match statement.kind {
            StatementKind::Assign(..) | StatementKind::SetDiscriminant { .. } => {
                self.super_statement(statement, location);
            }
            StatementKind::FakeRead(FakeReadCause::ForMatchedPlace, _) => {
                self.check_op(ops::IfOrMatch);
            }
            // FIXME(eddyb) should these really do nothing?
            StatementKind::FakeRead(..) |
            StatementKind::StorageLive(_) |
            StatementKind::StorageDead(_) |
            StatementKind::InlineAsm {..} |
            StatementKind::Retag { .. } |
            StatementKind::AscribeUserType(..) |
            StatementKind::Nop => {}
        }
    }

    fn visit_terminator_kind(&mut self, kind: &TerminatorKind<'tcx>, location: Location) {
        trace!("visit_terminator_kind: kind={:?} location={:?}", kind, location);
        self.super_terminator_kind(kind, location);

        match kind {
            TerminatorKind::Call { func, .. } => {
                let fn_ty = func.ty(self.body, self.tcx);

                let def_id = match fn_ty.kind {
                    ty::FnDef(def_id, _) => def_id,

                    ty::FnPtr(_) => {
                        self.check_op(ops::FnCallIndirect);
                        return;
                    }
                    _ => {
                        self.check_op(ops::FnCallOther);
                        return;
                    }
                };

                // At this point, we are calling a function whose `DefId` is known...

                if let Abi::RustIntrinsic | Abi::PlatformIntrinsic = self.tcx.fn_sig(def_id).abi() {
                    assert!(!self.tcx.is_const_fn(def_id));

                    if self.tcx.item_name(def_id) == sym::transmute {
                        self.check_op(ops::Transmute);
                        return;
                    }

                    // To preserve the current semantics, we return early, allowing all
                    // intrinsics (except `transmute`) to pass unchecked to miri.
                    //
                    // FIXME: We should keep a whitelist of allowed intrinsics (or at least a
                    // blacklist of unimplemented ones) and fail here instead.
                    return;
                }

                if self.tcx.is_const_fn(def_id) {
                    return;
                }

                if is_lang_panic_fn(self.tcx, def_id) {
                    self.check_op(ops::Panic);
                } else if let Some(feature) = self.tcx.is_unstable_const_fn(def_id) {
                    // Exempt unstable const fns inside of macros with
                    // `#[allow_internal_unstable]`.
                    if !self.span.allows_unstable(feature) {
                        self.check_op(ops::FnCallUnstable(def_id, feature));
                    }
                } else {
                    self.check_op(ops::FnCallNonConst(def_id));
                }

            }

            // Forbid all `Drop` terminators unless the place being dropped is a local with no
            // projections that cannot be `NeedsDrop`.
            | TerminatorKind::Drop { location: dropped_place, .. }
            | TerminatorKind::DropAndReplace { location: dropped_place, .. }
            => {
                let mut err_span = self.span;

                // Check to see if the type of this place can ever have a drop impl. If not, this
                // `Drop` terminator is frivolous.
                let ty_needs_drop = dropped_place
                    .ty(self.body, self.tcx)
                    .ty
                    .needs_drop(self.tcx, self.param_env);

                if !ty_needs_drop {
                    return;
                }

                let needs_drop = if let Some(local) = dropped_place.as_local() {
                    // Use the span where the local was declared as the span of the drop error.
                    err_span = self.body.local_decls[local].source_info.span;
                    self.qualifs.needs_drop_lazy_seek(local, location)
                } else {
                    true
                };

                if needs_drop {
                    self.check_op_spanned(ops::LiveDrop, err_span);
                }
            }

            _ => {}
        }
    }
}

fn error_min_const_fn_violation(tcx: TyCtxt<'_>, span: Span, msg: Cow<'_, str>) {
    struct_span_err!(tcx.sess, span, E0723, "{}", msg)
        .note("for more information, see issue https://github.com/rust-lang/rust/issues/57563")
        .help("add `#![feature(const_fn)]` to the crate attributes to enable")
        .emit();
}

fn check_short_circuiting_in_const_local(item: &Item<'_, 'tcx>) {
    let body = item.body;

    if body.control_flow_destroyed.is_empty() {
        return;
    }

    let mut locals = body.vars_iter();
    if let Some(local) = locals.next() {
        let span = body.local_decls[local].source_info.span;
        let mut error = item.tcx.sess.struct_span_err(
            span,
            &format!(
                "new features like let bindings are not permitted in {}s \
                which also use short circuiting operators",
                item.const_kind(),
            ),
        );
        for (span, kind) in body.control_flow_destroyed.iter() {
            error.span_note(
                *span,
                &format!("use of {} here does not actually short circuit due to \
                the const evaluator presently not being able to do control flow. \
                See https://github.com/rust-lang/rust/issues/49146 for more \
                information.", kind),
            );
        }
        for local in locals {
            let span = body.local_decls[local].source_info.span;
            error.span_note(span, "more locals defined here");
        }
        error.emit();
    }
}

fn check_return_ty_is_sync(tcx: TyCtxt<'tcx>, body: &Body<'tcx>, hir_id: HirId) {
    let ty = body.return_ty();
    tcx.infer_ctxt().enter(|infcx| {
        let cause = traits::ObligationCause::new(body.span, hir_id, traits::SharedStatic);
        let mut fulfillment_cx = traits::FulfillmentContext::new();
        let sync_def_id = tcx.require_lang_item(lang_items::SyncTraitLangItem, Some(body.span));
        fulfillment_cx.register_bound(&infcx, ty::ParamEnv::empty(), ty, sync_def_id, cause);
        if let Err(err) = fulfillment_cx.select_all_or_error(&infcx) {
            infcx.report_fulfillment_errors(&err, None, false);
        }
    });
}

fn place_as_reborrow(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    place: &'a Place<'tcx>,
) -> Option<&'a [PlaceElem<'tcx>]> {
    place
        .projection
        .split_last()
        .and_then(|(outermost, inner)| {
            if outermost != &ProjectionElem::Deref {
                return None;
            }

            // A borrow of a `static` also looks like `&(*_1)` in the MIR, but `_1` is a `const`
            // that points to the allocation for the static. Don't treat these as reborrows.
            if let PlaceBase::Local(local) = place.base {
                if body.local_decls[local].is_ref_to_static() {
                    return None;
                }
            }

            // Ensure the type being derefed is a reference and not a raw pointer.
            //
            // This is sufficient to prevent an access to a `static mut` from being marked as a
            // reborrow, even if the check above were to disappear.
            let inner_ty = Place::ty_from(&place.base, inner, body, tcx).ty;
            match inner_ty.kind {
                ty::Ref(..) => Some(inner),
                _ => None,
            }
        })
}
