//! FIXME: write short doc here

use std::sync::Arc;

use hir_def::{path::path, resolver::HasResolver, AdtId, FunctionId};
use hir_expand::diagnostics::DiagnosticSink;
use ra_syntax::{ast, AstPtr};
use rustc_hash::FxHashSet;

use crate::{
    db::HirDatabase,
    diagnostics::{
        MismatchedArgCount, MissingFields, MissingMatchArms, MissingOkInTailExpr, MissingPatFields,
    },
    utils::variant_data,
    ApplicationTy, InferenceResult, Ty, TypeCtor,
    _match::{is_useful, MatchCheckCtx, Matrix, PatStack, Usefulness},
};

pub use hir_def::{
    body::{
        scope::{ExprScopes, ScopeEntry, ScopeId},
        Body, BodySourceMap, ExprPtr, ExprSource, PatPtr, PatSource,
    },
    expr::{
        ArithOp, Array, BinaryOp, BindingAnnotation, CmpOp, Expr, ExprId, Literal, LogicOp,
        MatchArm, Ordering, Pat, PatId, RecordFieldPat, RecordLitField, Statement, UnaryOp,
    },
    src::HasSource,
    LocalFieldId, Lookup, VariantId,
};

pub struct ExprValidator<'a, 'b: 'a> {
    func: FunctionId,
    infer: Arc<InferenceResult>,
    sink: &'a mut DiagnosticSink<'b>,
}

impl<'a, 'b> ExprValidator<'a, 'b> {
    pub fn new(
        func: FunctionId,
        infer: Arc<InferenceResult>,
        sink: &'a mut DiagnosticSink<'b>,
    ) -> ExprValidator<'a, 'b> {
        ExprValidator { func, infer, sink }
    }

    pub fn validate_body(&mut self, db: &dyn HirDatabase) {
        let body = db.body(self.func.into());

        for (id, expr) in body.exprs.iter() {
            if let Some((variant_def, missed_fields, true)) =
                record_literal_missing_fields(db, &self.infer, id, expr)
            {
                self.create_record_literal_missing_fields_diagnostic(
                    id,
                    db,
                    variant_def,
                    missed_fields,
                );
            }

            match expr {
                Expr::Match { expr, arms } => {
                    self.validate_match(id, *expr, arms, db, self.infer.clone());
                }
                Expr::Call { .. } | Expr::MethodCall { .. } => {
                    self.validate_call(db, id, expr);
                }
                _ => {}
            }
        }
        for (id, pat) in body.pats.iter() {
            if let Some((variant_def, missed_fields, true)) =
                record_pattern_missing_fields(db, &self.infer, id, pat)
            {
                self.create_record_pattern_missing_fields_diagnostic(
                    id,
                    db,
                    variant_def,
                    missed_fields,
                );
            }
        }
        let body_expr = &body[body.body_expr];
        if let Expr::Block { tail: Some(t), .. } = body_expr {
            self.validate_results_in_tail_expr(body.body_expr, *t, db);
        }
    }

    fn create_record_literal_missing_fields_diagnostic(
        &mut self,
        id: ExprId,
        db: &dyn HirDatabase,
        variant_def: VariantId,
        missed_fields: Vec<LocalFieldId>,
    ) {
        // XXX: only look at source_map if we do have missing fields
        let (_, source_map) = db.body_with_source_map(self.func.into());

        if let Ok(source_ptr) = source_map.expr_syntax(id) {
            let root = source_ptr.file_syntax(db.upcast());
            if let ast::Expr::RecordLit(record_lit) = &source_ptr.value.to_node(&root) {
                if let Some(field_list) = record_lit.record_field_list() {
                    let variant_data = variant_data(db.upcast(), variant_def);
                    let missed_fields = missed_fields
                        .into_iter()
                        .map(|idx| variant_data.fields()[idx].name.clone())
                        .collect();
                    self.sink.push(MissingFields {
                        file: source_ptr.file_id,
                        field_list: AstPtr::new(&field_list),
                        missed_fields,
                    })
                }
            }
        }
    }

    fn create_record_pattern_missing_fields_diagnostic(
        &mut self,
        id: PatId,
        db: &dyn HirDatabase,
        variant_def: VariantId,
        missed_fields: Vec<LocalFieldId>,
    ) {
        // XXX: only look at source_map if we do have missing fields
        let (_, source_map) = db.body_with_source_map(self.func.into());

        if let Ok(source_ptr) = source_map.pat_syntax(id) {
            if let Some(expr) = source_ptr.value.as_ref().left() {
                let root = source_ptr.file_syntax(db.upcast());
                if let ast::Pat::RecordPat(record_pat) = expr.to_node(&root) {
                    if let Some(field_list) = record_pat.record_field_pat_list() {
                        let variant_data = variant_data(db.upcast(), variant_def);
                        let missed_fields = missed_fields
                            .into_iter()
                            .map(|idx| variant_data.fields()[idx].name.clone())
                            .collect();
                        self.sink.push(MissingPatFields {
                            file: source_ptr.file_id,
                            field_list: AstPtr::new(&field_list),
                            missed_fields,
                        })
                    }
                }
            }
        }
    }

    fn validate_call(&mut self, db: &dyn HirDatabase, call_id: ExprId, expr: &Expr) -> Option<()> {
        // Check that the number of arguments matches the number of parameters.

        // FIXME: Due to shortcomings in the current type system implementation, only emit this
        // diagnostic if there are no type mismatches in the containing function.
        if self.infer.type_mismatches.iter().next().is_some() {
            return Some(());
        }

        let is_method_call = matches!(expr, Expr::MethodCall { .. });
        let (callee, args) = match expr {
            Expr::Call { callee, args } => {
                let callee = &self.infer.type_of_expr[*callee];
                let (callable, _) = callee.as_callable()?;

                (callable, args.clone())
            }
            Expr::MethodCall { receiver, args, .. } => {
                let callee = self.infer.method_resolution(call_id)?;
                let mut args = args.clone();
                args.insert(0, *receiver);
                (callee.into(), args)
            }
            _ => return None,
        };

        let sig = db.callable_item_signature(callee);
        let params = sig.value.params();

        let mut param_count = params.len();
        let mut arg_count = args.len();

        if arg_count != param_count {
            let (_, source_map) = db.body_with_source_map(self.func.into());
            if let Ok(source_ptr) = source_map.expr_syntax(call_id) {
                if is_method_call {
                    param_count -= 1;
                    arg_count -= 1;
                }
                self.sink.push(MismatchedArgCount {
                    file: source_ptr.file_id,
                    call_expr: source_ptr.value,
                    expected: param_count,
                    found: arg_count,
                });
            }
        }

        None
    }

    fn validate_match(
        &mut self,
        id: ExprId,
        match_expr: ExprId,
        arms: &[MatchArm],
        db: &dyn HirDatabase,
        infer: Arc<InferenceResult>,
    ) {
        let (body, source_map): (Arc<Body>, Arc<BodySourceMap>) =
            db.body_with_source_map(self.func.into());

        let match_expr_ty = match infer.type_of_expr.get(match_expr) {
            Some(ty) => ty,
            // If we can't resolve the type of the match expression
            // we cannot perform exhaustiveness checks.
            None => return,
        };

        let cx = MatchCheckCtx { match_expr, body, infer: infer.clone(), db };
        let pats = arms.iter().map(|arm| arm.pat);

        let mut seen = Matrix::empty();
        for pat in pats {
            if let Some(pat_ty) = infer.type_of_pat.get(pat) {
                // We only include patterns whose type matches the type
                // of the match expression. If we had a InvalidMatchArmPattern
                // diagnostic or similar we could raise that in an else
                // block here.
                //
                // When comparing the types, we also have to consider that rustc
                // will automatically de-reference the match expression type if
                // necessary.
                //
                // FIXME we should use the type checker for this.
                if pat_ty == match_expr_ty
                    || match_expr_ty
                        .as_reference()
                        .map(|(match_expr_ty, _)| match_expr_ty == pat_ty)
                        .unwrap_or(false)
                {
                    // If we had a NotUsefulMatchArm diagnostic, we could
                    // check the usefulness of each pattern as we added it
                    // to the matrix here.
                    let v = PatStack::from_pattern(pat);
                    seen.push(&cx, v);
                    continue;
                }
            }

            // If we can't resolve the type of a pattern, or the pattern type doesn't
            // fit the match expression, we skip this diagnostic. Skipping the entire
            // diagnostic rather than just not including this match arm is preferred
            // to avoid the chance of false positives.
            return;
        }

        match is_useful(&cx, &seen, &PatStack::from_wild()) {
            Ok(Usefulness::Useful) => (),
            // if a wildcard pattern is not useful, then all patterns are covered
            Ok(Usefulness::NotUseful) => return,
            // this path is for unimplemented checks, so we err on the side of not
            // reporting any errors
            _ => return,
        }

        if let Ok(source_ptr) = source_map.expr_syntax(id) {
            let root = source_ptr.file_syntax(db.upcast());
            if let ast::Expr::MatchExpr(match_expr) = &source_ptr.value.to_node(&root) {
                if let (Some(match_expr), Some(arms)) =
                    (match_expr.expr(), match_expr.match_arm_list())
                {
                    self.sink.push(MissingMatchArms {
                        file: source_ptr.file_id,
                        match_expr: AstPtr::new(&match_expr),
                        arms: AstPtr::new(&arms),
                    })
                }
            }
        }
    }

    fn validate_results_in_tail_expr(&mut self, body_id: ExprId, id: ExprId, db: &dyn HirDatabase) {
        // the mismatch will be on the whole block currently
        let mismatch = match self.infer.type_mismatch_for_expr(body_id) {
            Some(m) => m,
            None => return,
        };

        let core_result_path = path![core::result::Result];

        let resolver = self.func.resolver(db.upcast());
        let core_result_enum = match resolver.resolve_known_enum(db.upcast(), &core_result_path) {
            Some(it) => it,
            _ => return,
        };

        let core_result_ctor = TypeCtor::Adt(AdtId::EnumId(core_result_enum));
        let params = match &mismatch.expected {
            Ty::Apply(ApplicationTy { ctor, parameters }) if ctor == &core_result_ctor => {
                parameters
            }
            _ => return,
        };

        if params.len() == 2 && params[0] == mismatch.actual {
            let (_, source_map) = db.body_with_source_map(self.func.into());

            if let Ok(source_ptr) = source_map.expr_syntax(id) {
                self.sink
                    .push(MissingOkInTailExpr { file: source_ptr.file_id, expr: source_ptr.value });
            }
        }
    }
}

pub fn record_literal_missing_fields(
    db: &dyn HirDatabase,
    infer: &InferenceResult,
    id: ExprId,
    expr: &Expr,
) -> Option<(VariantId, Vec<LocalFieldId>, /*exhaustive*/ bool)> {
    let (fields, exhausitve) = match expr {
        Expr::RecordLit { path: _, fields, spread } => (fields, spread.is_none()),
        _ => return None,
    };

    let variant_def = infer.variant_resolution_for_expr(id)?;
    if let VariantId::UnionId(_) = variant_def {
        return None;
    }

    let variant_data = variant_data(db.upcast(), variant_def);

    let specified_fields: FxHashSet<_> = fields.iter().map(|f| &f.name).collect();
    let missed_fields: Vec<LocalFieldId> = variant_data
        .fields()
        .iter()
        .filter_map(|(f, d)| if specified_fields.contains(&d.name) { None } else { Some(f) })
        .collect();
    if missed_fields.is_empty() {
        return None;
    }
    Some((variant_def, missed_fields, exhausitve))
}

pub fn record_pattern_missing_fields(
    db: &dyn HirDatabase,
    infer: &InferenceResult,
    id: PatId,
    pat: &Pat,
) -> Option<(VariantId, Vec<LocalFieldId>, /*exhaustive*/ bool)> {
    let (fields, exhaustive) = match pat {
        Pat::Record { path: _, args, ellipsis } => (args, !ellipsis),
        _ => return None,
    };

    let variant_def = infer.variant_resolution_for_pat(id)?;
    if let VariantId::UnionId(_) = variant_def {
        return None;
    }

    let variant_data = variant_data(db.upcast(), variant_def);

    let specified_fields: FxHashSet<_> = fields.iter().map(|f| &f.name).collect();
    let missed_fields: Vec<LocalFieldId> = variant_data
        .fields()
        .iter()
        .filter_map(|(f, d)| if specified_fields.contains(&d.name) { None } else { Some(f) })
        .collect();
    if missed_fields.is_empty() {
        return None;
    }
    Some((variant_def, missed_fields, exhaustive))
}

#[cfg(test)]
mod tests {
    use expect::{expect, Expect};
    use ra_db::fixture::WithFixture;

    use crate::{diagnostics::MismatchedArgCount, test_db::TestDB};

    fn check_diagnostic(ra_fixture: &str, expect: Expect) {
        let msg = TestDB::with_single_file(ra_fixture).0.diagnostic::<MismatchedArgCount>().0;
        expect.assert_eq(&msg);
    }

    fn check_no_diagnostic(ra_fixture: &str) {
        let (s, diagnostic_count) =
            TestDB::with_single_file(ra_fixture).0.diagnostic::<MismatchedArgCount>();

        assert_eq!(0, diagnostic_count, "expected no diagnostic, found one: {}", s);
    }

    #[test]
    fn simple_free_fn_zero() {
        check_diagnostic(
            r"
            fn zero() {}
            fn f() { zero(1); }
            ",
            expect![["\"zero(1)\": Expected 0 arguments, found 1\n"]],
        );

        check_no_diagnostic(
            r"
            fn zero() {}
            fn f() { zero(); }
            ",
        );
    }

    #[test]
    fn simple_free_fn_one() {
        check_diagnostic(
            r"
            fn one(arg: u8) {}
            fn f() { one(); }
            ",
            expect![["\"one()\": Expected 1 argument, found 0\n"]],
        );

        check_no_diagnostic(
            r"
            fn one(arg: u8) {}
            fn f() { one(1); }
            ",
        );
    }

    #[test]
    fn method_as_fn() {
        check_diagnostic(
            r"
            struct S;
            impl S {
                fn method(&self) {}
            }

            fn f() {
                S::method();
            }
            ",
            expect![["\"S::method()\": Expected 1 argument, found 0\n"]],
        );

        check_no_diagnostic(
            r"
            struct S;
            impl S {
                fn method(&self) {}
            }

            fn f() {
                S::method(&S);
                S.method();
            }
            ",
        );
    }

    #[test]
    fn method_with_arg() {
        check_diagnostic(
            r"
            struct S;
            impl S {
                fn method(&self, arg: u8) {}
            }

            fn f() {
                S.method();
            }
            ",
            expect![["\"S.method()\": Expected 1 argument, found 0\n"]],
        );

        check_no_diagnostic(
            r"
            struct S;
            impl S {
                fn method(&self, arg: u8) {}
            }

            fn f() {
                S::method(&S, 0);
                S.method(1);
            }
            ",
        );
    }

    #[test]
    fn tuple_struct() {
        check_diagnostic(
            r"
            struct Tup(u8, u16);
            fn f() {
                Tup(0);
            }
            ",
            expect![["\"Tup(0)\": Expected 2 arguments, found 1\n"]],
        )
    }

    #[test]
    fn enum_variant() {
        check_diagnostic(
            r"
            enum En {
                Variant(u8, u16),
            }
            fn f() {
                En::Variant(0);
            }
            ",
            expect![["\"En::Variant(0)\": Expected 2 arguments, found 1\n"]],
        )
    }
}
