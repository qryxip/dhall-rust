use std::borrow::Cow;
use std::cmp::max;
use std::collections::HashMap;

use crate::error::{TypeError, TypeMessage};
use crate::semantics::phase::normalize::merge_maps;
use crate::semantics::phase::Normalized;
use crate::semantics::{
    type_of_builtin, Binder, BuiltinClosure, Closure, TyEnv, TyExpr,
    TyExprKind, Type, Value, ValueKind,
};
use crate::syntax::{
    BinOp, Builtin, Const, Expr, ExprKind, InterpolatedTextContents, Span,
};

fn type_of_recordtype<'a>(
    tys: impl Iterator<Item = Cow<'a, TyExpr>>,
) -> Result<Type, TypeError> {
    // An empty record type has type Type
    let mut k = Const::Type;
    for t in tys {
        match t.get_type()?.as_const() {
            Some(c) => k = max(k, c),
            None => return mkerr("InvalidFieldType"),
        }
    }
    Ok(Value::from_const(k))
}

fn function_check(a: Const, b: Const) -> Const {
    if b == Const::Type {
        Const::Type
    } else {
        max(a, b)
    }
}

fn type_of_function(src: Type, tgt: Type) -> Result<Type, TypeError> {
    let ks = match src.as_const() {
        Some(k) => k,
        _ => return Err(TypeError::new(TypeMessage::InvalidInputType(src))),
    };
    let kt = match tgt.as_const() {
        Some(k) => k,
        _ => return Err(TypeError::new(TypeMessage::InvalidOutputType(tgt))),
    };

    Ok(Value::from_const(function_check(ks, kt)))
}

fn mkerr<T, S: ToString>(x: S) -> Result<T, TypeError> {
    Err(TypeError::new(TypeMessage::Custom(x.to_string())))
}

/// When all sub-expressions have been typed, check the remaining toplevel
/// layer.
fn type_one_layer(
    env: &TyEnv,
    kind: &ExprKind<TyExpr, Normalized>,
) -> Result<Type, TypeError> {
    Ok(match kind {
        ExprKind::Import(..) => unreachable!(
            "There should remain no imports in a resolved expression"
        ),
        ExprKind::Var(..)
        | ExprKind::Lam(..)
        | ExprKind::Pi(..)
        | ExprKind::Let(..)
        | ExprKind::Const(Const::Sort)
        | ExprKind::Embed(..) => unreachable!(), // Handled in type_with

        ExprKind::Const(Const::Type) => Value::from_const(Const::Kind),
        ExprKind::Const(Const::Kind) => Value::from_const(Const::Sort),
        ExprKind::Builtin(b) => {
            let t_expr = type_of_builtin(*b);
            let t_tyexpr = type_with(env, &t_expr)?;
            t_tyexpr.normalize_whnf(env.as_nzenv())
        }
        ExprKind::BoolLit(_) => Value::from_builtin(Builtin::Bool),
        ExprKind::NaturalLit(_) => Value::from_builtin(Builtin::Natural),
        ExprKind::IntegerLit(_) => Value::from_builtin(Builtin::Integer),
        ExprKind::DoubleLit(_) => Value::from_builtin(Builtin::Double),
        ExprKind::TextLit(interpolated) => {
            let text_type = Value::from_builtin(Builtin::Text);
            for contents in interpolated.iter() {
                use InterpolatedTextContents::Expr;
                if let Expr(x) = contents {
                    if x.get_type()? != text_type {
                        return mkerr("InvalidTextInterpolation");
                    }
                }
            }
            text_type
        }
        ExprKind::EmptyListLit(t) => {
            let t = t.normalize_nf(env.as_nzenv());
            match &*t.kind() {
                ValueKind::AppliedBuiltin(BuiltinClosure {
                    b: Builtin::List,
                    args,
                    ..
                }) if args.len() == 1 => {}
                _ => return mkerr("InvalidListType"),
            };
            t
        }
        ExprKind::NEListLit(xs) => {
            let mut iter = xs.iter();
            let x = iter.next().unwrap();
            for y in iter {
                if x.get_type()? != y.get_type()? {
                    return mkerr("InvalidListElement");
                }
            }
            let t = x.get_type()?;
            if t.get_type()?.as_const() != Some(Const::Type) {
                return mkerr("InvalidListType");
            }

            Value::from_builtin(Builtin::List).app(t)
        }
        ExprKind::SomeLit(x) => {
            let t = x.get_type()?;
            if t.get_type()?.as_const() != Some(Const::Type) {
                return mkerr("InvalidOptionalType");
            }

            Value::from_builtin(Builtin::Optional).app(t)
        }
        ExprKind::RecordLit(kvs) => {
            use std::collections::hash_map::Entry;
            let mut kts = HashMap::new();
            for (x, v) in kvs {
                // Check for duplicated entries
                match kts.entry(x.clone()) {
                    Entry::Occupied(_) => {
                        return mkerr("RecordTypeDuplicateField")
                    }
                    Entry::Vacant(e) => e.insert(v.get_type()?),
                };
            }

            let ty = type_of_recordtype(
                kts.iter()
                    .map(|(_, t)| Cow::Owned(t.to_tyexpr(env.as_varenv()))),
            )?;
            Value::from_kind_and_type(ValueKind::RecordType(kts), ty)
        }
        ExprKind::RecordType(kts) => {
            use std::collections::hash_map::Entry;
            let mut seen_fields = HashMap::new();
            for (x, _) in kts {
                // Check for duplicated entries
                match seen_fields.entry(x.clone()) {
                    Entry::Occupied(_) => {
                        return mkerr("RecordTypeDuplicateField")
                    }
                    Entry::Vacant(e) => e.insert(()),
                };
            }

            type_of_recordtype(kts.iter().map(|(_, t)| Cow::Borrowed(t)))?
        }
        ExprKind::UnionType(kts) => {
            use std::collections::hash_map::Entry;
            let mut seen_fields = HashMap::new();
            // Check that all types are the same const
            let mut k = None;
            for (x, t) in kts {
                if let Some(t) = t {
                    match (k, t.get_type()?.as_const()) {
                        (None, Some(k2)) => k = Some(k2),
                        (Some(k1), Some(k2)) if k1 == k2 => {}
                        _ => return mkerr("InvalidFieldType"),
                    }
                }
                match seen_fields.entry(x) {
                    Entry::Occupied(_) => {
                        return mkerr("UnionTypeDuplicateField")
                    }
                    Entry::Vacant(e) => e.insert(()),
                };
            }

            // An empty union type has type Type;
            // an union type with only unary variants also has type Type
            let k = k.unwrap_or(Const::Type);

            Value::from_const(k)
        }
        ExprKind::Field(scrut, x) => {
            match &*scrut.get_type()?.kind() {
                ValueKind::RecordType(kts) => match kts.get(&x) {
                    Some(tth) => tth.clone(),
                    None => return mkerr("MissingRecordField"),
                },
                // TODO: branch here only when scrut.get_type() is a Const
                _ => {
                    let scrut_nf = scrut.normalize_nf(env.as_nzenv());
                    let scrut_nf_borrow = scrut_nf.kind();
                    match &*scrut_nf_borrow {
                        ValueKind::UnionType(kts) => match kts.get(x) {
                            // Constructor has type T -> < x: T, ... >
                            Some(Some(ty)) => Value::from_kind_and_type(
                                ValueKind::PiClosure {
                                    binder: Binder::new(x.clone()),
                                    annot: ty.clone(),
                                    closure: Closure::new_constant(
                                        env.as_nzenv(),
                                        scrut.clone(),
                                    ),
                                },
                                type_of_function(
                                    ty.get_type()?,
                                    scrut.get_type()?,
                                )?,
                            ),
                            Some(None) => scrut_nf.clone(),
                            None => return mkerr("MissingUnionField"),
                        },
                        _ => return mkerr("NotARecord"),
                    }
                } // _ => mkerr("NotARecord"),
            }
        }
        ExprKind::Annot(x, t) => {
            let t = t.normalize_whnf(env.as_nzenv());
            let x_ty = x.get_type()?;
            if x_ty != t {
                return mkerr(format!(
                    "annot mismatch: ({} : {}) : {}",
                    x.to_expr_tyenv(env),
                    x_ty.to_tyexpr(env.as_varenv()).to_expr_tyenv(env),
                    t.to_tyexpr(env.as_varenv()).to_expr_tyenv(env)
                ));
                // return mkerr(format!(
                //     "annot mismatch: {} != {}",
                //     x_ty.to_tyexpr(env.as_varenv()).to_expr_tyenv(env),
                //     t.to_tyexpr(env.as_varenv()).to_expr_tyenv(env)
                // ));
                // return mkerr(format!("annot mismatch: {:#?} : {:#?}", x, t,));
            }
            x_ty
        }
        ExprKind::Assert(t) => {
            let t = t.normalize_whnf(env.as_nzenv());
            match &*t.kind() {
                ValueKind::Equivalence(x, y) if x == y => {}
                ValueKind::Equivalence(..) => return mkerr("AssertMismatch"),
                _ => return mkerr("AssertMustTakeEquivalence"),
            }
            t
        }
        ExprKind::App(f, arg) => {
            let tf = f.get_type()?;
            let tf_borrow = tf.kind();
            match &*tf_borrow {
                ValueKind::PiClosure { annot, closure, .. } => {
                    if arg.get_type()? != *annot {
                        // return mkerr(format!("function annot mismatch"));
                        return mkerr(format!(
                            "function annot mismatch: ({} : {}) : {}",
                            arg.to_expr_tyenv(env),
                            arg.get_type()?
                                .to_tyexpr(env.as_varenv())
                                .to_expr_tyenv(env),
                            annot.to_tyexpr(env.as_varenv()).to_expr_tyenv(env),
                        ));
                    }

                    let arg_nf = arg.normalize_nf(env.as_nzenv());
                    closure.apply(arg_nf)
                }
                _ => return mkerr(format!("apply to not Pi")),
            }
        }
        ExprKind::BoolIf(x, y, z) => {
            if *x.get_type()?.kind() != ValueKind::from_builtin(Builtin::Bool) {
                return mkerr("InvalidPredicate");
            }
            if y.get_type()?.get_type()?.as_const() != Some(Const::Type) {
                return mkerr("IfBranchMustBeTerm");
            }
            if z.get_type()?.get_type()?.as_const() != Some(Const::Type) {
                return mkerr("IfBranchMustBeTerm");
            }
            if y.get_type()? != z.get_type()? {
                return mkerr("IfBranchMismatch");
            }

            y.get_type()?
        }
        ExprKind::BinOp(BinOp::RightBiasedRecordMerge, x, y) => {
            let x_type = x.get_type()?;
            let y_type = y.get_type()?;

            // Extract the LHS record type
            let x_type_borrow = x_type.kind();
            let kts_x = match &*x_type_borrow {
                ValueKind::RecordType(kts) => kts,
                _ => return mkerr("MustCombineRecord"),
            };

            // Extract the RHS record type
            let y_type_borrow = y_type.kind();
            let kts_y = match &*y_type_borrow {
                ValueKind::RecordType(kts) => kts,
                _ => return mkerr("MustCombineRecord"),
            };

            // Union the two records, prefering
            // the values found in the RHS.
            let kts = merge_maps::<_, _, _, !>(kts_x, kts_y, |_, _, r_t| {
                Ok(r_t.clone())
            })?;

            // Construct the final record type
            let ty = type_of_recordtype(
                kts.iter()
                    .map(|(_, t)| Cow::Owned(t.to_tyexpr(env.as_varenv()))),
            )?;
            Value::from_kind_and_type(ValueKind::RecordType(kts), ty)
        }
        ExprKind::BinOp(BinOp::RecursiveRecordMerge, x, y) => {
            let ekind = ExprKind::BinOp(
                BinOp::RecursiveRecordTypeMerge,
                x.get_type()?.to_tyexpr(env.as_varenv()),
                y.get_type()?.to_tyexpr(env.as_varenv()),
            );
            let ty = type_one_layer(env, &ekind)?;
            TyExpr::new(TyExprKind::Expr(ekind), Some(ty), Span::Artificial)
                .normalize_nf(env.as_nzenv())
        }
        ExprKind::BinOp(BinOp::RecursiveRecordTypeMerge, x, y) => {
            let x_val = x.normalize_whnf(env.as_nzenv());
            let y_val = y.normalize_whnf(env.as_nzenv());
            let x_val_borrow = x_val.kind();
            let y_val_borrow = y_val.kind();
            let kts_x = match &*x_val_borrow {
                ValueKind::RecordType(kts) => kts,
                _ => return mkerr("RecordTypeMergeRequiresRecordType"),
            };
            let kts_y = match &*y_val_borrow {
                ValueKind::RecordType(kts) => kts,
                _ => return mkerr("RecordTypeMergeRequiresRecordType"),
            };
            for (k, tx) in kts_x {
                if let Some(ty) = kts_y.get(k) {
                    type_one_layer(
                        env,
                        &ExprKind::BinOp(
                            BinOp::RecursiveRecordTypeMerge,
                            tx.to_tyexpr(env.as_varenv()),
                            ty.to_tyexpr(env.as_varenv()),
                        ),
                    )?;
                }
            }

            // A RecordType's type is always a const
            let xk = x.get_type()?.as_const().unwrap();
            let yk = y.get_type()?.as_const().unwrap();
            Value::from_const(max(xk, yk))
        }
        ExprKind::BinOp(BinOp::ListAppend, l, r) => {
            let l_ty = l.get_type()?;
            match &*l_ty.kind() {
                ValueKind::AppliedBuiltin(BuiltinClosure {
                    b: Builtin::List,
                    ..
                }) => {}
                _ => return mkerr("BinOpTypeMismatch"),
            }

            if l_ty != r.get_type()? {
                return mkerr("BinOpTypeMismatch");
            }

            l_ty
        }
        ExprKind::BinOp(BinOp::Equivalence, l, r) => {
            if l.get_type()? != r.get_type()? {
                return mkerr("EquivalenceTypeMismatch");
            }
            if l.get_type()?.get_type()?.as_const() != Some(Const::Type) {
                return mkerr("EquivalenceArgumentsMustBeTerms");
            }

            Value::from_const(Const::Type)
        }
        ExprKind::BinOp(o, l, r) => {
            let t = Value::from_builtin(match o {
                BinOp::BoolAnd
                | BinOp::BoolOr
                | BinOp::BoolEQ
                | BinOp::BoolNE => Builtin::Bool,
                BinOp::NaturalPlus | BinOp::NaturalTimes => Builtin::Natural,
                BinOp::TextAppend => Builtin::Text,
                BinOp::ListAppend
                | BinOp::RightBiasedRecordMerge
                | BinOp::RecursiveRecordMerge
                | BinOp::RecursiveRecordTypeMerge
                | BinOp::Equivalence => unreachable!(),
                BinOp::ImportAlt => unreachable!("ImportAlt leftover in tck"),
            });

            if l.get_type()? != t {
                return mkerr("BinOpTypeMismatch");
            }

            if r.get_type()? != t {
                return mkerr("BinOpTypeMismatch");
            }

            t
        }
        ExprKind::Merge(record, union, type_annot) => {
            let record_type = record.get_type()?;
            let record_borrow = record_type.kind();
            let handlers = match &*record_borrow {
                ValueKind::RecordType(kts) => kts,
                _ => return mkerr("Merge1ArgMustBeRecord"),
            };

            let union_type = union.get_type()?;
            let union_borrow = union_type.kind();
            let variants = match &*union_borrow {
                ValueKind::UnionType(kts) => Cow::Borrowed(kts),
                ValueKind::AppliedBuiltin(BuiltinClosure {
                    b: Builtin::Optional,
                    args,
                    ..
                }) if args.len() == 1 => {
                    let ty = &args[0];
                    let mut kts = HashMap::new();
                    kts.insert("None".into(), None);
                    kts.insert("Some".into(), Some(ty.clone()));
                    Cow::Owned(kts)
                }
                _ => return mkerr("Merge2ArgMustBeUnionOrOptional"),
            };

            let mut inferred_type = None;
            for (x, handler_type) in handlers {
                let handler_return_type = match variants.get(x) {
                    // Union alternative with type
                    Some(Some(variant_type)) => {
                        let handler_type_borrow = handler_type.kind();
                        match &*handler_type_borrow {
                            ValueKind::PiClosure { closure, annot, .. } => {
                                if variant_type != annot {
                                    return mkerr("MergeHandlerTypeMismatch");
                                }

                                closure.remove_binder().or_else(|()| {
                                    mkerr("MergeReturnTypeIsDependent")
                                })?
                            }
                            _ => return mkerr("NotAFunction"),
                        }
                    }
                    // Union alternative without type
                    Some(None) => handler_type.clone(),
                    None => return mkerr("MergeHandlerMissingVariant"),
                };
                match &inferred_type {
                    None => inferred_type = Some(handler_return_type),
                    Some(t) => {
                        if t != &handler_return_type {
                            return mkerr("MergeHandlerTypeMismatch");
                        }
                    }
                }
            }
            for x in variants.keys() {
                if !handlers.contains_key(x) {
                    return mkerr("MergeVariantMissingHandler");
                }
            }

            let type_annot = type_annot
                .as_ref()
                .map(|t| t.normalize_whnf(env.as_nzenv()));
            match (inferred_type, type_annot) {
                (Some(t1), Some(t2)) => {
                    if t1 != t2 {
                        return mkerr("MergeAnnotMismatch");
                    }
                    t1
                }
                (Some(t), None) => t,
                (None, Some(t)) => t,
                (None, None) => return mkerr("MergeEmptyNeedsAnnotation"),
            }
        }
        ExprKind::ToMap(_, _) => unimplemented!("toMap"),
        ExprKind::Projection(record, labels) => {
            let record_type = record.get_type()?;
            let record_type_borrow = record_type.kind();
            let kts = match &*record_type_borrow {
                ValueKind::RecordType(kts) => kts,
                _ => return mkerr("ProjectionMustBeRecord"),
            };

            let mut new_kts = HashMap::new();
            for l in labels {
                match kts.get(l) {
                    None => return mkerr("ProjectionMissingEntry"),
                    Some(t) => {
                        use std::collections::hash_map::Entry;
                        match new_kts.entry(l.clone()) {
                            Entry::Occupied(_) => {
                                return mkerr("ProjectionDuplicateField")
                            }
                            Entry::Vacant(e) => e.insert(t.clone()),
                        }
                    }
                };
            }

            Value::from_kind_and_type(
                ValueKind::RecordType(new_kts),
                record_type.get_type()?,
            )
        }
        ExprKind::ProjectionByExpr(_, _) => {
            unimplemented!("selection by expression")
        }
        ExprKind::Completion(_, _) => unimplemented!("record completion"),
    })
}

/// `type_with` typechecks an expressio in the provided environment.
pub(crate) fn type_with(
    env: &TyEnv,
    expr: &Expr<Normalized>,
) -> Result<TyExpr, TypeError> {
    let (tyekind, ty) = match expr.as_ref() {
        ExprKind::Var(var) => match env.lookup(&var) {
            Some((k, ty)) => (k, Some(ty)),
            None => return mkerr("unbound variable"),
        },
        ExprKind::Lam(binder, annot, body) => {
            let annot = type_with(env, annot)?;
            let annot_nf = annot.normalize_nf(env.as_nzenv());
            let body_env = env.insert_type(&binder, annot_nf.clone());
            let body = type_with(&body_env, body)?;
            let body_ty = body.get_type()?;
            let ty = TyExpr::new(
                TyExprKind::Expr(ExprKind::Pi(
                    binder.clone(),
                    annot.clone(),
                    body_ty.to_tyexpr(body_env.as_varenv()),
                )),
                Some(type_of_function(annot.get_type()?, body_ty.get_type()?)?),
                Span::Artificial,
            );
            let ty = ty.normalize_whnf(env.as_nzenv());
            (
                TyExprKind::Expr(ExprKind::Lam(binder.clone(), annot, body)),
                Some(ty),
            )
        }
        ExprKind::Pi(binder, annot, body) => {
            let annot = type_with(env, annot)?;
            let annot_nf = annot.normalize_whnf(env.as_nzenv());
            let body =
                type_with(&env.insert_type(binder, annot_nf.clone()), body)?;
            let ty = type_of_function(annot.get_type()?, body.get_type()?)?;
            (
                TyExprKind::Expr(ExprKind::Pi(binder.clone(), annot, body)),
                Some(ty),
            )
        }
        ExprKind::Let(binder, annot, val, body) => {
            let val = if let Some(t) = annot {
                t.rewrap(ExprKind::Annot(val.clone(), t.clone()))
            } else {
                val.clone()
            };

            let val = type_with(env, &val)?;
            let val_nf = val.normalize_nf(&env.as_nzenv());
            let body = type_with(&env.insert_value(&binder, val_nf), body)?;
            let body_ty = body.get_type().ok();
            (
                TyExprKind::Expr(ExprKind::Let(
                    binder.clone(),
                    None,
                    val,
                    body,
                )),
                body_ty,
            )
        }
        ExprKind::Const(Const::Sort) => {
            (TyExprKind::Expr(ExprKind::Const(Const::Sort)), None)
        }
        ExprKind::Embed(p) => {
            return Ok(p.clone().into_value().to_tyexpr_noenv())
        }
        ekind => {
            let ekind = ekind.traverse_ref(|e| type_with(env, e))?;
            let ty = type_one_layer(env, &ekind)?;
            (TyExprKind::Expr(ekind), Some(ty))
        }
    };

    Ok(TyExpr::new(tyekind, ty, expr.span()))
}

/// Typecheck an expression and return the expression annotated with types if type-checking
/// succeeded, or an error if type-checking failed.
pub(crate) fn typecheck(e: &Expr<Normalized>) -> Result<TyExpr, TypeError> {
    type_with(&TyEnv::new(), e)
}

/// Like `typecheck`, but additionally checks that the expression's type matches the provided type.
pub(crate) fn typecheck_with(
    expr: &Expr<Normalized>,
    ty: Expr<Normalized>,
) -> Result<TyExpr, TypeError> {
    typecheck(&expr.rewrap(ExprKind::Annot(expr.clone(), ty)))
}
