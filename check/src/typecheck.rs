//! The main typechecking interface which is responsible for typechecking expressions, patterns,
//! etc. Only checks which need to be aware of expressions are handled here the actual unifying and
//! checking of types are done in the `unify_type` and `kindcheck` modules.
use std::borrow::Cow;
use std::fmt;
use std::iter::once;
use std::mem;
use std::sync::Arc;

use itertools::Itertools;

use base::scoped_map::ScopedMap;
use base::ast::{DisplayEnv, Expr, Literal, MutVisitor, Pattern, PatternField, SpannedExpr};
use base::ast::{AstType, SpannedIdent, SpannedPattern, TypeBinding, ValueBinding};
use base::error::Errors;
use base::fnv::{FnvMap, FnvSet};
use base::resolve;
use base::kind::{ArcKind, Kind, KindCache, KindEnv};
use base::merge;
use base::pos::{BytePos, Span, Spanned};
use base::symbol::{Symbol, SymbolModule, SymbolRef, Symbols};
use base::types::{self, Alias, AliasData, AppVec, ArcType, Field, Generic, PrimitiveEnv,
                  RecordSelector, Skolem, Type, TypeCache, TypeEnv, TypeVariable};

use kindcheck::{self, Error as KindCheckError, KindCheck, KindError};
use substitution::{self, Constraints, Substitution};
use rename::RenameError;
use unify::{self, Error as UnifyError};
use unify_type::{self, new_skolem_scope, Error as UnifyTypeError};

/// Type representing a single error when checking a type
#[derive(Debug, PartialEq)]
pub enum TypeError<I> {
    /// Variable has not been defined before it was used
    UndefinedVariable(I),
    /// Attempt to call a type which is not a function
    NotAFunction(ArcType<I>),
    /// Type has not been defined before it was used
    UndefinedType(I),
    /// Type were expected to have a certain field
    UndefinedField(ArcType<I>, I),
    /// Constructor type was found in a pattern but did not have the expected number of arguments
    PatternError(ArcType<I>, usize),
    /// Errors found when trying to unify two types
    Unification(ArcType<I>, ArcType<I>, Vec<UnifyTypeError<I>>),
    /// Error were found when trying to unify the kinds of two types
    KindError(KindCheckError<I>),
    /// Errors found during renaming (overload resolution)
    Rename(RenameError),
    /// Multiple types were declared with the same name in the same expression
    DuplicateTypeDefinition(I),
    /// A field was defined more than once in a record constructor or pattern match
    DuplicateField(String),
    /// Type is not a type which has any fields
    InvalidProjection(ArcType<I>),
    /// Expected to find a record with the following fields
    UndefinedRecord { fields: Vec<I> },
    /// Found a case expression without any alternatives
    EmptyCase,
    /// An `Error` ast node was found indicating an invalid parse
    ErrorAst(&'static str),
    Message(String),
}

impl<I> From<KindCheckError<I>> for TypeError<I>
where
    I: PartialEq + Clone,
{
    fn from(e: KindCheckError<I>) -> TypeError<I> {
        match e {
            UnifyError::Other(KindError::UndefinedType(name)) => TypeError::UndefinedType(name),
            e => TypeError::KindError(e),
        }
    }
}

impl<I> From<RenameError> for TypeError<I> {
    fn from(e: RenameError) -> TypeError<I> {
        TypeError::Rename(e)
    }
}

impl<I: fmt::Display + AsRef<str>> fmt::Display for TypeError<I> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::TypeError::*;
        use pretty::{Arena, DocAllocator};
        match *self {
            UndefinedVariable(ref name) => write!(f, "Undefined variable `{}`", name),
            NotAFunction(ref typ) => write!(f, "`{}` is not a function", typ),
            UndefinedType(ref name) => write!(f, "Type `{}` is not defined", name),
            UndefinedField(ref typ, ref field) => {
                write!(f, "Type `{}` does not have the field `{}`", typ, field)
            }
            Unification(ref expected, ref actual, ref errors) => {
                let arena = Arena::new();
                let types = chain![&arena;
                    "Expected:",
                    chain![&arena;
                        arena.space(),
                        expected.pretty(&arena)
                    ].nest(4).group(),
                    arena.newline(),
                    "Found:",
                    chain![&arena;
                        arena.space(),
                        actual.pretty(&arena)
                    ].nest(4).group()
                ].group();
                let doc = chain![&arena;
                    "Expected the following types to be equal",
                    arena.newline(),
                    types,
                    arena.newline(),
                    arena.as_string(errors.len()),
                    " errors were found during unification:"
                ];
                writeln!(f, "{}", doc.1.pretty(80))?;
                if errors.is_empty() {
                    return Ok(());
                }
                for error in &errors[..errors.len() - 1] {
                    writeln!(f, "{}", error)?;
                }
                write!(f, "{}", errors.last().unwrap())
            }
            PatternError(ref typ, expected_len) => {
                write!(f, "Type {} has {} to few arguments", typ, expected_len)
            }
            KindError(ref err) => kindcheck::fmt_kind_error(err, f),
            Rename(ref err) => write!(f, "{}", err),
            DuplicateTypeDefinition(ref id) => write!(
                f,
                "Type '{}' has been already been defined in this module",
                id
            ),
            DuplicateField(ref id) => {
                write!(f, "The record has more than one field named '{}'", id)
            }
            InvalidProjection(ref typ) => write!(
                f,
                "Type '{}' is not a type which allows field accesses",
                typ
            ),
            UndefinedRecord { ref fields } => {
                write!(f, "No type found with the following fields: ")?;
                write!(f, "{}", fields[0])?;
                for field in &fields[1..] {
                    write!(f, ", {}", field)?;
                }
                Ok(())
            }
            EmptyCase => write!(f, "`case` expression with no alternatives"),
            ErrorAst(typ) => write!(f, "`Error` {} found during typechecking", typ),
            Message(ref msg) => write!(f, "{}", msg),
        }
    }
}

pub type SpannedTypeError<Id> = Spanned<TypeError<Id>, BytePos>;

type TcResult<T> = Result<T, TypeError<Symbol>>;

#[derive(Debug)]
struct StackBinding {
    constraints: FnvMap<Symbol, Constraints<ArcType>>,
    typ: ArcType,
}

struct Environment<'a> {
    /// The global environment which the typechecker extracts types from
    environment: &'a (PrimitiveEnv + 'a),
    /// Stack allocated variables
    stack: ScopedMap<Symbol, StackBinding>,
    /// Types which exist in some scope (`type Test = ... in ...`)
    stack_types: ScopedMap<Symbol, (ArcType, Alias<Symbol, ArcType>)>,
}

impl<'a> KindEnv for Environment<'a> {
    fn find_kind(&self, type_name: &SymbolRef) -> Option<ArcKind> {
        self.stack_types
            .get(type_name)
            .map(|&(_, ref alias)| {
                let mut kind = Kind::typ();
                for arg in alias.params().iter().rev() {
                    kind = Kind::function(arg.kind.clone(), kind);
                }
                kind
            })
            .or_else(|| self.environment.find_kind(type_name))
    }
}

impl<'a> TypeEnv for Environment<'a> {
    fn find_type(&self, id: &SymbolRef) -> Option<&ArcType> {
        self.stack
            .get(id)
            .map(|bind| &bind.typ)
            .or_else(|| self.environment.find_type(id))
    }

    fn find_type_info(&self, id: &SymbolRef) -> Option<&Alias<Symbol, ArcType>> {
        self.stack_types
            .get(id)
            .map(|&(_, ref alias)| alias)
            .or_else(|| self.environment.find_type_info(id))
    }

    fn find_record(
        &self,
        fields: &[Symbol],
        selector: RecordSelector,
    ) -> Option<(ArcType, ArcType)> {
        self.stack_types
            .iter()
            .find(|&(_, &(_, ref alias))| match **alias.unresolved_type() {
                Type::Record(ref row) => {
                        let record_fields = || row.row_iter()
                            .map(|f| f.name.name())
                            .chain(row.type_field_iter ().map(|f| f.name.name()));
                        selector.matches(record_fields, fields.iter().map(|field| field.name()))
                    }
                    _ => false,
                }
            )
            // FIXME Don't use unresolved_type
            .map(|t| ((t.1).0.clone(), (t.1).1.unresolved_type().clone()))
            .or_else(|| self.environment.find_record(fields, selector))
    }
}

impl<'a> PrimitiveEnv for Environment<'a> {
    fn get_bool(&self) -> &ArcType {
        self.environment.get_bool()
    }
}

/// Type returned from the main typecheck function to make sure that nested `type` and `let`
/// expressions dont overflow the stack
enum TailCall {
    Type(ArcType),
    /// Returned from typechecking a `let` or `type` expresion to indicate that the expression body
    /// should be typechecked now.
    TailCall,
}

/// Struct which provides methods to typecheck expressions.
pub struct Typecheck<'a> {
    environment: Environment<'a>,
    symbols: SymbolModule<'a>,
    /// Mapping from the fresh symbol generated during typechecking to the symbol that was assigned
    /// during typechecking
    original_symbols: ScopedMap<Symbol, Symbol>,
    subs: Substitution<ArcType>,
    named_variables: FnvMap<Symbol, ArcType>,
    errors: Errors<SpannedTypeError<Symbol>>,
    /// Type variables `let test: a -> b` (`a` and `b`)
    type_variables: ScopedMap<Symbol, ArcType>,
    type_cache: TypeCache<Symbol, ArcType>,
    kind_cache: KindCache,
}

/// Error returned when unsuccessfully typechecking an expression
pub type Error = Errors<SpannedTypeError<Symbol>>;

impl<'a> Typecheck<'a> {
    /// Create a new typechecker which typechecks expressions in `module`
    pub fn new(
        module: String,
        symbols: &'a mut Symbols,
        environment: &'a (PrimitiveEnv + 'a),
        type_cache: TypeCache<Symbol, ArcType>,
    ) -> Typecheck<'a> {
        let symbols = SymbolModule::new(module, symbols);
        let kind_cache = KindCache::new();
        Typecheck {
            environment: Environment {
                environment: environment,
                stack: ScopedMap::new(),
                stack_types: ScopedMap::new(),
            },
            symbols: symbols,
            original_symbols: ScopedMap::new(),
            subs: Substitution::new(kind_cache.typ()),
            named_variables: FnvMap::default(),
            errors: Errors::new(),
            type_variables: ScopedMap::new(),
            type_cache: type_cache,
            kind_cache: kind_cache,
        }
    }

    fn error(&mut self, span: Span<BytePos>, error: TypeError<Symbol>) -> ArcType {
        self.errors.push(Spanned {
            span: span,
            value: error,
        });
        self.subs.new_var()
    }

    fn bool(&self) -> ArcType {
        self.environment.get_bool().clone()
    }

    fn find_at(&mut self, span: Span<BytePos>, id: &Symbol) -> ArcType {
        match self.find(id) {
            Ok(typ) => typ,
            Err(err) => self.error(span, err),
        }
    }

    fn find(&mut self, id: &Symbol) -> TcResult<ArcType> {
        match self.environment.find_type(id).map(ArcType::clone) {
            Some(typ) => {
                let constraints = self.environment
                    .stack
                    .get(id)
                    .map(|bind| Cow::Borrowed(&bind.constraints))
                    .unwrap_or_else(|| Cow::Owned(FnvMap::default()));
                let typ = self.subs.set_type(typ);

                self.named_variables.clear();
                let typ = new_skolem_scope(&self.subs, &constraints, &typ);
                debug!("Find {} : {:?}", self.symbols.string(id), typ);
                Ok(typ)
            }
            None => Err(TypeError::UndefinedVariable(id.clone())),
        }
    }

    fn find_record(
        &self,
        fields: &[Symbol],
        selector: RecordSelector,
    ) -> TcResult<(ArcType, ArcType)> {
        // If fields is empty it is going to match any record which means this function probably
        // returns the wrong record as the record we expect can still contain type fields.
        // Just return an error so that inference continues without any guessed record type.
        if fields.is_empty() {
            Err(TypeError::UndefinedRecord {
                fields: fields.to_owned(),
            })
        } else {
            self.environment
                .find_record(fields, selector)
                .ok_or(TypeError::UndefinedRecord {
                    fields: fields.to_owned(),
                })
        }
    }

    fn find_type_info(&self, id: &Symbol) -> TcResult<&Alias<Symbol, ArcType>> {
        self.environment
            .find_type_info(id)
            .ok_or_else(|| TypeError::UndefinedType(id.clone()))
    }

    fn stack_var(&mut self, id: Symbol, typ: ArcType) {
        debug!("Insert {} : {:?}", id, typ);
        self.environment.stack.insert(
            id,
            StackBinding {
                constraints: FnvMap::default(),
                typ: typ,
            },
        );
    }

    fn stack_type(&mut self, id: Symbol, alias: &Alias<Symbol, ArcType>) {
        // Insert variant constructors into the local scope
        let aliased_type = alias.typ();
        if let Type::Variant(ref row) = **aliased_type.remove_forall() {
            for field in row.row_iter().cloned() {
                let symbol = self.symbols.symbol(field.name.as_ref());
                self.original_symbols.insert(symbol, field.name.clone());
                self.stack_var(
                    field.name,
                    Type::forall(alias.params().to_owned(), field.typ),
                );
            }
        }
        let generic_args = alias.params().iter().cloned().map(Type::generic).collect();
        let typ = Type::<_, ArcType>::app(alias.as_ref().clone(), generic_args);
        {
            // FIXME: Workaround so that both the types name in this module and its global
            // name are imported. Without this aliases may not be traversed properly
            self.environment
                .stack_types
                .insert(alias.name.clone(), (typ.clone(), alias.clone()));
        }
        self.environment
            .stack_types
            .insert(id, (typ, alias.clone()));
    }

    fn enter_scope(&mut self) {
        self.environment.stack.enter_scope();
        self.environment.stack_types.enter_scope();
        self.original_symbols.enter_scope();
    }

    fn exit_scope(&mut self) {
        self.environment.stack.exit_scope();
        self.environment.stack_types.exit_scope();
        self.original_symbols.exit_scope();
    }

    fn generalize_binding(&mut self, level: u32, binding: &mut ValueBinding<Symbol>) {
        self.generalize_type(level, &mut binding.resolved_type);
        self.generalize_variables(level, &mut binding.args, &mut binding.expr)
    }

    /// Generalizing updates all variables which are above `level` into "generic variables". A
    /// generic variable is instantiated with a fresh type variable each time it is refered to.
    ///
    /// Generalzing is a crucial part when inferring types as types will otherwise be less general
    /// than they need to. Consider the following expression.
    /// ```f#
    /// let id x = x
    /// in id 2
    /// ```
    /// If the variable in `id` was not generalized before the `id 2` call the variable would be
    /// unified to an `Int` which is not what we want. Generalazing before checking the body of the
    /// `let` basically infers that the variables in `id` does not refer to anything outside the
    /// `let` scope and can thus be "generalized" into `a -> a` which is instantiated with a fresh
    /// type variable in the `id 2` call.
    fn generalize_variables(
        &mut self,
        level: u32,
        args: &mut [SpannedIdent<Symbol>],
        expr: &mut SpannedExpr<Symbol>,
    ) {
        self.type_variables.enter_scope();

        // Replaces all type variables with their inferred types
        struct ReplaceVisitor<'a, 'b: 'a> {
            level: u32,
            tc: &'a mut Typecheck<'b>,
        }

        impl<'a, 'b> MutVisitor for ReplaceVisitor<'a, 'b> {
            type Ident = Symbol;

            fn visit_typ(&mut self, typ: &mut ArcType) {
                if let Some(finished) = self.tc.finish_type(self.level, typ) {
                    *typ = finished;
                }
            }
        }
        {
            let mut visitor = ReplaceVisitor {
                level: level,
                tc: self,
            };
            visitor.visit_expr(expr);
            for arg in args {
                visitor.visit_typ(&mut arg.value.typ)
            }
        }

        self.type_variables.exit_scope();
    }

    fn generalize_type_errors(&mut self, errors: &mut Error) {
        self.type_variables.enter_scope();

        for err in errors {
            use self::TypeError::*;

            match err.value {
                UndefinedVariable(_) |
                UndefinedType(_) |
                DuplicateTypeDefinition(_) |
                DuplicateField(_) |
                UndefinedRecord { .. } |
                EmptyCase |
                ErrorAst(_) |
                Rename(_) |
                KindError(_) |
                Message(_) => (),
                NotAFunction(ref mut typ) |
                UndefinedField(ref mut typ, _) |
                PatternError(ref mut typ, _) |
                InvalidProjection(ref mut typ) => {
                    self.generalize_type(0, typ);
                }
                Unification(ref mut expected, ref mut actual, ref mut errors) => {
                    self.generalize_type(0, expected);
                    self.generalize_type(0, actual);
                    for err in errors {
                        match *err {
                            unify::Error::TypeMismatch(ref mut l, ref mut r) => {
                                self.generalize_type(0, l);
                                self.generalize_type(0, r);
                            }
                            unify::Error::Substitution(ref mut err) => match *err {
                                substitution::Error::Occurs(_, ref mut typ) => {
                                    self.generalize_type(0, typ);
                                }
                                substitution::Error::Constraint(
                                    ref mut var,
                                    ref mut constraints,
                                ) => {
                                    for typ in Arc::make_mut(constraints) {
                                        self.generalize_type(0, typ);
                                    }
                                    self.generalize_type(0, var);
                                }
                            },
                            unify::Error::Other(ref mut err) => {
                                if let unify_type::TypeError::MissingFields(ref mut typ, _) = *err {
                                    self.generalize_type(0, typ);
                                }
                            }
                        }
                    }
                }
            }
        }

        self.type_variables.exit_scope();
    }

    fn generalize_type(&mut self, level: u32, typ: &mut ArcType) {
        let mut generic = None;
        let mut i = 0;

        self.type_variables.enter_scope();
        let result_type = self.finish_type_(level, &mut generic, &mut i, typ);
        let variables = self.type_variables.exit_scope();
        let result_type = result_type.map(|typ| {
            Type::forall(
                variables
                    .map(|(id, typ)| {
                        Generic {
                            id,
                            kind: typ.kind().into_owned(),
                        }
                    })
                    .collect(),
                typ,
            )
        });
        if let Some(finished) = result_type {
            *typ = finished;
        }
    }

    /// Typecheck `expr`. If successful the type of the expression will be returned and all
    /// identifiers in `expr` will be filled with the inferred type
    pub fn typecheck_expr(&mut self, expr: &mut SpannedExpr<Symbol>) -> Result<ArcType, Error> {
        self.typecheck_expr_expected(expr, None)
    }

    pub fn typecheck_expr_expected(
        &mut self,
        expr: &mut SpannedExpr<Symbol>,
        expected_type: Option<&ArcType>,
    ) -> Result<ArcType, Error> {
        fn tail_expr(e: &mut SpannedExpr<Symbol>) -> &mut SpannedExpr<Symbol> {
            match e.value {
                Expr::LetBindings(_, ref mut b) | Expr::TypeBindings(_, ref mut b) => tail_expr(b),
                _ => e,
            }
        }
        self.subs.clear();
        self.environment.stack.clear();

        let mut typ = self.typecheck_opt(expr, expected_type);
        if let Some(expected) = expected_type {
            let expected = self.create_unifiable_signature(expected)
                .unwrap_or_else(|| expected.clone());
            typ = self.merge_signature(expr_check_span(expr), 0, &expected, typ);
        }
        typ = self.finish_type(0, &typ).unwrap_or(typ);
        typ = types::walk_move_type(typ, &mut unroll_typ);
        // Only the 'tail' expression need to be generalized at this point as all bindings
        // will have already been generalized
        self.generalize_variables(0, &mut [], tail_expr(expr));

        if self.errors.has_errors() {
            let mut errors = mem::replace(&mut self.errors, Errors::new());
            self.generalize_type_errors(&mut errors);
            Err(errors)
        } else {
            match ::rename::rename(&mut self.symbols, &self.environment, expr) {
                Ok(()) => {
                    debug!("Typecheck result: {}", typ);
                    Ok(typ)
                }
                Err(errors) => {
                    for Spanned { span, value } in errors {
                        self.errors.push(Spanned {
                            span: span,
                            value: value.into(),
                        });
                    }
                    Err(mem::replace(&mut self.errors, Errors::new()))
                }
            }
        }
    }

    fn infer_expr(&mut self, expr: &mut SpannedExpr<Symbol>) -> ArcType {
        self.typecheck_opt(expr, None)
    }


    fn typecheck(&mut self, expr: &mut SpannedExpr<Symbol>, expected_type: &ArcType) -> ArcType {
        self.typecheck_opt(expr, Some(expected_type))
    }

    /// Main typechecking function. Returns the type of the expression if typechecking was
    /// successful
    fn typecheck_opt(
        &mut self,
        mut expr: &mut SpannedExpr<Symbol>,
        expected_type: Option<&ArcType>,
    ) -> ArcType {
        fn moving<T>(t: T) -> T {
            t
        }
        // How many scopes that have been entered in this "tailcall" loop
        let mut scope_count = 0;
        let returned_type;
        loop {
            match self.typecheck_(expr, expected_type) {
                Ok(tailcall) => {
                    match tailcall {
                        TailCall::TailCall => {
                            // Call typecheck_ again with the next expression
                            expr = match moving(expr).value {
                                Expr::LetBindings(_, ref mut new_expr) |
                                Expr::TypeBindings(_, ref mut new_expr) => new_expr,
                                _ => panic!("Only Let and Type expressions can tailcall"),
                            };
                            scope_count += 1;
                        }
                        TailCall::Type(typ) => {
                            returned_type = typ;
                            break;
                        }
                    }
                }
                Err(err) => {
                    returned_type = self.subs.new_var();
                    self.errors.push(Spanned {
                        span: expr_check_span(expr),
                        value: err,
                    });
                    break;
                }
            }
        }
        for _ in 0..scope_count {
            self.exit_scope();
        }
        returned_type
    }

    fn typecheck_(
        &mut self,
        expr: &mut SpannedExpr<Symbol>,
        expected_type: Option<&ArcType<Symbol>>,
    ) -> Result<TailCall, TypeError<Symbol>> {
        match expr.value {
            Expr::Ident(ref mut id) => {
                if let Some(new) = self.original_symbols.get(&id.name) {
                    id.name = new.clone();
                }
                id.typ = self.find(&id.name)?;
                Ok(TailCall::Type(id.typ.clone()))
            }
            Expr::Literal(ref lit) => Ok(TailCall::Type(match *lit {
                Literal::Int(_) => self.type_cache.int(),
                Literal::Byte(_) => self.type_cache.byte(),
                Literal::Float(_) => self.type_cache.float(),
                Literal::String(_) => self.type_cache.string(),
                Literal::Char(_) => self.type_cache.char(),
            })),
            Expr::App(ref mut func, ref mut args) => {
                let mut func_type = self.infer_expr(&mut **func);
                for arg in args.iter_mut() {
                    let f = self.type_cache
                        .function(once(self.subs.new_var()), self.subs.new_var());
                    func_type = self.instantiate_generics(&func_type);
                    func_type = self.unify(&f, func_type)?;
                    func_type = match func_type.as_function() {
                        Some((arg_ty, ret_ty)) => {
                            let actual = self.typecheck(arg, arg_ty);
                            self.unify_span(expr_check_span(arg), arg_ty, actual);
                            ret_ty.clone()
                        }
                        None => return Err(TypeError::NotAFunction(func_type.clone())),
                    };
                }
                Ok(TailCall::Type(func_type))
            }
            Expr::IfElse(ref mut pred, ref mut if_true, ref mut if_false) => {
                let bool_type = self.bool();
                let pred_type = self.typecheck(&mut **pred, &bool_type);
                self.unify_span(expr_check_span(pred), &bool_type, pred_type);

                // Both branches must unify to the same type
                let true_type = self.typecheck_opt(&mut **if_true, expected_type);
                let false_type = self.typecheck_opt(&mut **if_false, expected_type);
                self.unify(&true_type, false_type).map(TailCall::Type)
            }
            Expr::Infix(ref mut lhs, ref mut op, ref mut rhs) => {
                let lhs_type = self.infer_expr(&mut **lhs);
                let rhs_type = self.infer_expr(&mut **rhs);
                let op_name = String::from(self.symbols.string(&op.value.name));
                let return_type = if op_name.starts_with('#') {
                    // Handle primitives
                    let arg_type = self.unify(&lhs_type, rhs_type)?;
                    let op_type = op_name.trim_matches(|c: char| !c.is_alphabetic());
                    let builtin_type = op_type.parse().map_err(|_| {
                        TypeError::Message("Invalid builtin type for operator".to_string())
                    })?;
                    let prim_type = self.type_cache.builtin_type(builtin_type);
                    let typ = self.unify(&prim_type, arg_type)?;
                    let return_type = match &op_name[1 + op_type.len()..] {
                        "+" | "-" | "*" | "/" => typ,
                        "==" | "<" => self.bool(),
                        _ => return Err(TypeError::UndefinedVariable(op.value.name.clone())),
                    };
                    op.value.typ = self.type_cache.function(
                        vec![prim_type.clone(), prim_type.clone()],
                        return_type.clone(),
                    );
                    return_type
                } else {
                    match &*op_name {
                        "&&" | "||" => {
                            self.unify(&lhs_type, rhs_type.clone())?;
                            op.value.typ = self.type_cache
                                .function(vec![self.bool(), self.bool()], self.bool());
                            self.unify(&self.bool(), lhs_type)?
                        }
                        _ => {
                            op.value.typ = self.find(&op.value.name)?;
                            op.value.typ = self.instantiate_generics(&op.value.typ);
                            let func_type = self.type_cache
                                .function(vec![lhs_type, rhs_type], self.subs.new_var());
                            let ret = self.unify(&op.value.typ, func_type)?
                                .as_function()
                                .and_then(|(_, ret)| ret.as_function())
                                .map(|(_, ret)| ret.clone())
                                .expect("ICE: unify binop");

                            ret
                        }
                    }
                };
                Ok(TailCall::Type(return_type))
            }
            Expr::Tuple {
                ref mut typ,
                elems: ref mut exprs,
            } => {
                *typ = match exprs.len() {
                    0 => Type::unit(),
                    1 => self.typecheck_opt(&mut exprs[0], expected_type),
                    _ => {
                        let fields = exprs
                            .iter_mut()
                            .enumerate()
                            .map(|(i, expr)| {
                                let typ = self.infer_expr(expr);
                                Field {
                                    name: self.symbols.symbol(format!("_{}", i)),
                                    typ: typ,
                                }
                            })
                            .collect();
                        Type::record(vec![], fields)
                    }
                };
                Ok(TailCall::Type(typ.clone()))
            }
            Expr::Match(ref mut expr, ref mut alts) => {
                let typ = self.infer_expr(&mut **expr);
                let mut expected_alt_type = None;

                for alt in alts.iter_mut() {
                    self.enter_scope();
                    self.typecheck_pattern(&mut alt.pattern, typ.clone());
                    let mut alt_type = self.typecheck_opt(&mut alt.expr, expected_type);
                    self.exit_scope();
                    // All alternatives must unify to the same type
                    if let Some(ref expected) = expected_alt_type {
                        alt_type = self.unify(expected, alt_type)?;
                    }
                    expected_alt_type = Some(alt_type);
                }
                expected_alt_type
                    .ok_or(TypeError::EmptyCase)
                    .map(TailCall::Type)
            }
            Expr::LetBindings(ref mut bindings, _) => {
                self.typecheck_bindings(bindings)?;
                Ok(TailCall::TailCall)
            }
            Expr::Projection(ref mut expr, ref field_id, ref mut ast_field_typ) => {
                let mut expr_typ = self.infer_expr(&mut **expr);
                debug!(
                    "Projection {} . {:?}",
                    &expr_typ,
                    self.symbols.string(field_id)
                );
                self.subs.make_real(&mut expr_typ);
                if let Type::Variable(_) = *expr_typ {
                    // Eagerly attempt to find a record with `field_access` since infering to just
                    // a polymorphic record may cause some code to fail to infer such as
                    // the test `row_polymorphism::late_merge_with_signature`
                    if let Ok(record_type) =
                        self.find_record(&[field_id.clone()], RecordSelector::Subset)
                            .map(|t| t.0.clone())
                    {
                        let record_type = self.instantiate(&record_type);
                        expr_typ = self.unify(&record_type, expr_typ)?;
                    }
                }
                expr_typ = self.instantiate_generics(&expr_typ);
                let record = self.remove_aliases(expr_typ.clone());
                match *record {
                    Type::Variable(_) | Type::Record(_) => {
                        let field_type = record
                            .row_iter()
                            .find(|field| field.name.name_eq(field_id))
                            .map(|field| field.typ.clone());
                        *ast_field_typ = match field_type {
                            Some(typ) => self.instantiate(&typ),
                            None => {
                                // FIXME As the polymorphic `record_type` do not have the type
                                // fields which `typ` this unification is only done after we
                                // checked if the field exists which lets field accesses on
                                // types with type fields still work
                                let field_var = self.subs.new_var();
                                let field = Field::new(field_id.clone(), field_var.clone());
                                let record_type =
                                    Type::poly_record(vec![], vec![field], self.subs.new_var());
                                self.unify(&record_type, record)?;
                                field_var
                            }
                        };
                        Ok(TailCall::Type(ast_field_typ.clone()))
                    }
                    _ => Err(TypeError::InvalidProjection(record)),
                }
            }
            Expr::Array(ref mut array) => {
                let mut expected_type = self.subs.new_var();
                for expr in &mut array.exprs {
                    let typ = self.typecheck(expr, &expected_type);
                    expected_type = self.unify_span(expr.span, &expected_type, typ);
                }
                array.typ = self.type_cache.array(expected_type);
                Ok(TailCall::Type(array.typ.clone()))
            }
            Expr::Lambda(ref mut lambda) => {
                let loc = format!("lambda:{}", expr.span.start);
                lambda.id.name = self.symbols.symbol(loc);
                let function_type = self.subs.new_var();
                let typ = self.typecheck_lambda(function_type, &mut lambda.args, &mut lambda.body);
                lambda.id.typ = typ.clone();
                Ok(TailCall::Type(typ))
            }
            Expr::TypeBindings(ref mut bindings, ref expr) => {
                self.typecheck_type_bindings(bindings, expr)?;
                Ok(TailCall::TailCall)
            }
            Expr::Record {
                ref mut typ,
                ref mut types,
                exprs: ref mut fields,
                ref mut base,
            } => {
                debug!(
                    "aaaaaaaaaa {:?}",
                    self.subs.find_type_for_var(8).map(|x| self.subs.real(x))
                );
                debug!("{:?}", self.environment.stack);
                let mut new_types: Vec<Field<_, _>> = Vec::with_capacity(types.len());

                let mut duplicated_fields = FnvSet::default();
                for field in types {
                    if let Some(ref mut typ) = field.value {
                        *typ = self.create_unifiable_signature(typ)
                            .unwrap_or_else(|| typ.clone());
                    }
                    let alias = self.find_type_info(&field.name.value)?.clone();
                    if self.error_on_duplicated_field(&mut duplicated_fields, field.name.clone()) {
                        new_types.push(Field::new(field.name.value.clone(), alias));
                    }
                }

                let mut new_fields: Vec<Field<_, _>> = Vec::with_capacity(fields.len());
                for field in fields {
                    let typ = match field.value {
                        Some(ref mut expr) => self.infer_expr(expr),
                        None => self.find(&field.name.value)?,
                    };
                    if self.error_on_duplicated_field(&mut duplicated_fields, field.name.clone()) {
                        new_fields.push(Field::new(field.name.value.clone(), typ));
                    }
                }

                if let Some(ref mut base) = *base {
                    let base_type = self.typecheck(base);
                    let base_type = self.remove_aliases(base_type);

                    let record_type = Type::poly_record(vec![], vec![], self.subs.new_var());
                    let base_type = self.unify_span(base.span, &record_type, base_type);

                    new_types.extend(
                        base_type
                            .type_field_iter()
                            .filter(|field| {
                                !duplicated_fields.contains(field.name.declared_name())
                            })
                            .cloned(),
                    );
                    new_fields.extend(
                        base_type
                            .row_iter()
                            .filter(|field| {
                                !duplicated_fields.contains(field.name.declared_name())
                            })
                            .cloned(),
                    );
                }

                let record_fields = new_fields
                    .iter()
                    .map(|f| f.name.clone())
                    .chain(new_types.iter().map(|f| f.name.clone()))
                    .collect::<Vec<_>>();
                let result = self.find_record(&record_fields, RecordSelector::Exact)
                    .map(|t| (t.0.clone(), t.1.clone()));
                let (id_type, record_type) = match result {
                    Ok(x) => x,
                    Err(_) => {
                        *typ = self.type_cache.record(new_types, new_fields);
                        return Ok(TailCall::Type(typ.clone()));
                    }
                };

                let id_type = self.instantiate(&id_type);
                let record_type = new_skolem_scope(&self.subs, &FnvMap::default(), &record_type);
                self.unify(&self.type_cache.record(new_types, new_fields), record_type)?;

                *typ = id_type.clone();
                Ok(TailCall::Type(id_type.clone()))
            }
            Expr::Block(ref mut exprs) => {
                let (last, exprs) = exprs.split_last_mut().expect("Expr in block");
                for expr in exprs {
                    self.infer_expr(expr);
                }
                Ok(TailCall::Type(self.typecheck_opt(last, expected_type)))
            }
            Expr::Error => Err(TypeError::ErrorAst("expression")),
        }
    }

    fn typecheck_lambda(
        &mut self,
        mut function_type: ArcType,
        args: &mut [SpannedIdent<Symbol>],
        body: &mut SpannedExpr<Symbol>,
    ) -> ArcType {
        self.enter_scope();
        println!(
            "----------- {};;;\n{:?}",
            function_type,
            self.type_variables
        );
        function_type = self.skolemize(&function_type);
        println!(">>{}", function_type);
        println!(">>{}", self.subs.set_type(function_type.clone()));
        let mut arg_types = Vec::new();
        let body_type = {
            let mut iter1 = function_arg_iter(self, function_type);
            let mut iter2 = args.iter_mut();
            while let (Some(arg_type), Some(arg)) = (iter1.next(), iter2.next()) {
                println!("{} {:?}", arg.value.name, arg_type);
                let arg = &mut arg.value;

                arg.typ = arg_type;
                arg_types.push(arg.typ.clone());
                iter1.tc.stack_var(arg.name.clone(), arg.typ.clone());
            }
            iter1.typ
        };
        let body_type = self.typecheck(body, &body_type);
        self.exit_scope();
        self.type_cache.function(arg_types, body_type)
    }

    fn typecheck_pattern(
        &mut self,
        pattern: &mut SpannedPattern<Symbol>,
        match_type: ArcType,
    ) -> ArcType {
        let span = pattern.span;
        match pattern.value {
            Pattern::Constructor(ref mut id, ref mut args) => {
                if let Some(new) = self.original_symbols.get(&id.name) {
                    id.name = new.clone();
                }
                // Find the enum constructor and return the types for its arguments
                let ctor_type = self.find_at(span, &id.name);
                id.typ = ctor_type.clone();
                let return_type = match self.typecheck_pattern_rec(args, ctor_type) {
                    Ok(return_type) => return_type,
                    Err(err) => self.error(span, err),
                };
                self.unify_span(span, &match_type, return_type)
            }
            Pattern::Record {
                typ: ref mut curr_typ,
                types: ref mut associated_types,
                ref mut fields,
            } => {
                *curr_typ = match_type.clone();

                let mut pattern_fields = Vec::with_capacity(associated_types.len() + fields.len());

                let mut duplicated_fields = FnvSet::default();
                {
                    let all_fields = associated_types
                        .iter()
                        .map(|field| &field.name)
                        .chain(fields.iter().map(|field| &field.name));
                    for field in all_fields {
                        if self.error_on_duplicated_field(&mut duplicated_fields, field.clone()) {
                            pattern_fields.push(field.value.clone());
                        }
                    }
                }

                // actual_type is the record (not hidden behind an alias)
                let record_guess = match *match_type {
                    // If the type we are matching on already an alias we don't guess as it is
                    // possible that we guess the wrong type (and we already have an alias anyway)
                    Type::Alias(_) | Type::Record(_) => None,
                    _ => self.find_record(&pattern_fields, RecordSelector::Subset)
                        .map(|t| (t.0.clone(), t.1.clone()))
                        .ok(),
                };
                let (mut typ, mut actual_type) = match record_guess {
                    Some(typ) => typ,
                    None => {
                        // HACK Since there is no way to unify just the name of the 'type field's we
                        // need to take the types from the matched type. This leaves the `types`
                        // list incomplete however since it may miss some fields defined in the
                        // pattern. These are catched later in this function.
                        let types = self.remove_alias(match_type.clone())
                            .type_field_iter()
                            .filter(|field| {
                                associated_types
                                    .iter()
                                    .any(|other| other.name.value.name_eq(&field.name))
                            })
                            .cloned()
                            .collect();

                        let fields = fields
                            .iter()
                            .map(|field| {
                                Field::new(field.name.value.clone(), self.subs.new_var())
                            })
                            .collect();
                        let t = Type::poly_record(types, fields, self.subs.new_var());
                        (t.clone(), t)
                    }
                };
                typ = self.instantiate(&typ);
                actual_type = new_skolem_scope(&self.subs, &FnvMap::default(), &actual_type);
                self.unify_span(span, &match_type, typ);
                let match_type = actual_type;

                for field in fields {
                    let name = &field.name.value;
                    // The field should always exist since the type was constructed from the pattern
                    let field_type = match_type
                        .row_iter()
                        .find(|f| f.name.name_eq(name))
                        .expect("ICE: Expected field to exist in type")
                        .typ
                        .clone();
                    match field.value {
                        Some(ref mut pattern) => {
                            self.typecheck_pattern(pattern, field_type);
                        }
                        None => {
                            self.stack_var(name.clone(), field_type);
                        }
                    }
                }

                // Check that all types declared in the pattern exists
                for field in associated_types.iter_mut() {
                    let name = field.value.as_ref().unwrap_or(&field.name.value).clone();
                    // The `types` in the record type should have a type matching the
                    // `name`
                    let field_type = match_type
                        .type_field_iter()
                        .find(|field| field.name.name_eq(&name));
                    match field_type {
                        Some(field_type) => {
                            // This forces refresh_type to remap the name a type was given
                            // in this module to its actual name
                            self.original_symbols
                                .insert(name.clone(), field_type.typ.name.clone());
                            self.stack_type(name, &field_type.typ);
                        }
                        None => {
                            self.error(span, TypeError::UndefinedField(match_type.clone(), name));
                        }
                    }
                }

                match_type
            }
            Pattern::Tuple {
                ref mut typ,
                ref mut elems,
            } => {
                let tuple_type = {
                    let subs = &mut self.subs;
                    self.type_cache
                        .tuple(&mut self.symbols, (0..elems.len()).map(|_| subs.new_var()))
                };
                *typ = self.unify_span(span, &tuple_type, match_type);
                for (elem, field) in elems.iter_mut().zip(tuple_type.row_iter()) {
                    self.typecheck_pattern(elem, field.typ.clone());
                }
                tuple_type
            }
            Pattern::Ident(ref mut id) => {
                self.stack_var(id.name.clone(), match_type.clone());
                id.typ = match_type.clone();
                match_type
            }
            Pattern::Error => self.error(span, TypeError::ErrorAst("pattern")),
        }
    }

    fn typecheck_pattern_rec(
        &mut self,
        args: &mut [SpannedPattern<Symbol>],
        typ: ArcType,
    ) -> TcResult<ArcType> {
        let len = args.len();
        match args.split_first_mut() {
            Some((head, tail)) => {
                let typ = self.instantiate_generics(&typ);
                match typ.as_function() {
                    Some((arg, ret)) => {
                        self.typecheck_pattern(head, arg.clone());
                        self.typecheck_pattern_rec(tail, ret.clone())
                    }
                    None => Err(TypeError::PatternError(typ.clone(), len)),
                }
            }
            None => Ok(typ),
        }
    }

    fn typecheck_bindings(&mut self, bindings: &mut [ValueBinding<Symbol>]) -> TcResult<()> {
        self.enter_scope();
        self.type_variables.enter_scope();
        let level = self.subs.var_id();

        for bind in bindings.iter_mut() {
            if let Some(ref typ) = bind.typ {
                bind.resolved_type = types::translate_type(&self.type_cache, typ);
            }
        }

        let is_recursive = bindings.iter().all(|bind| !bind.args.is_empty());
        // When the definitions are allowed to be mutually recursive
        if is_recursive {
            for bind in bindings.iter_mut() {
                match bind.name.value {
                    Pattern::Constructor(ref id, _) | Pattern::Ident(ref id)
                        if id.name.declared_name().starts_with(char::is_uppercase) =>
                    {
                        self.error(
                            bind.name.span,
                            TypeError::Message(
                                format!("Unexpected type constructor `{}`", id.name),
                            ),
                        );
                    }
                    _ => (),
                }
                let typ = {
                    bind.resolved_type = self.create_unifiable_signature(&bind.resolved_type)
                        .unwrap_or_else(|| bind.resolved_type.clone());
                    self.kindcheck(&mut bind.resolved_type)?;
                    self.instantiate_signature(&bind.resolved_type)
                };
                self.typecheck_pattern(&mut bind.name, typ);
                if let Expr::Lambda(ref mut lambda) = bind.expr.value {
                    if let Pattern::Ident(ref name) = bind.name.value {
                        lambda.id.name = name.name.clone();
                    }
                }
            }
        }
        let mut types = Vec::new();
        for bind in bindings.iter_mut() {
            self.type_variables.enter_scope();

            // Functions which are declared as `let f x = ...` are allowed to be self
            // recursive
            let mut typ = if bind.args.is_empty() {
                self.instantiate_signature(&bind.resolved_type);
                bind.resolved_type = self.create_unifiable_signature(&bind.resolved_type)
                    .unwrap_or_else(|| bind.resolved_type.clone());
                self.kindcheck(&mut bind.resolved_type)?;
                self.typecheck(&mut bind.expr, &bind.resolved_type)
            } else {
                let function_typ = self.instantiate(&bind.resolved_type);
                self.typecheck_lambda(function_typ, &mut bind.args, &mut bind.expr)
            };

            debug!("let {:?} : {}", bind.name, typ);

            typ = self.merge_signature(bind.name.span, level, &bind.resolved_type, typ);


            if !is_recursive {
                // Merge the type declaration and the actual type
                self.generalize_binding(level, bind);
                self.typecheck_pattern(&mut bind.name, typ);
            } else {
                types.push(typ);
            }

            self.type_variables.exit_scope();
        }
        // Once all variables inside the let has been unified we can quantify them
        debug!("Generalize {}", level);
        for bind in bindings.iter_mut() {
            self.generalize_binding(level, bind);
            self.finish_pattern(level, &mut bind.name, &bind.resolved_type);
        }
        debug!("Typecheck `in`");
        self.type_variables.exit_scope();
        Ok(())
    }

    fn typecheck_type_bindings(
        &mut self,
        bindings: &mut [TypeBinding<Symbol>],
        expr: &SpannedExpr<Symbol>,
    ) -> TcResult<()> {
        self.enter_scope();

        let mut resolved_aliases = Vec::new();
        // Rename the types so they get a name which is distinct from types from other
        // modules
        for bind in bindings.iter_mut() {
            let s = String::from(self.symbols.string(&bind.alias.value.name));
            let new = self.symbols.scoped_symbol(&s);
            self.original_symbols
                .insert(bind.alias.value.name.clone(), new.clone());
            // Rename the aliase's name to its global name
            bind.alias.value.name = new;

            fn check_undefined_variables(
                self_: &mut Typecheck,
                args: &[Generic<Symbol>],
                typ: &AstType<Symbol>,
            ) {
                use base::pos::HasSpan;
                match **typ {
                    Type::Generic(ref id) => if args.iter().all(|arg| arg.id != id.id) {
                        self_.error(
                            typ.span(),
                            TypeError::UndefinedVariable(id.id.clone()).into(),
                        );
                    },
                    Type::Record(_) => {
                        // Inside records variables are bound implicitly to the closest field
                        // so variables are allowed to be undefined/implicit
                    }
                    _ => {
                        types::walk_move_type_opt(
                            typ,
                            &mut types::ControlVisitation(|typ: &AstType<_>| {
                                check_undefined_variables(self_, args, typ);
                                None
                            }),
                        );
                    }
                }
            }
            check_undefined_variables(self, &bind.alias.value.args, bind.alias.value.unresolved_type());

            let alias = types::translate_alias(&self.type_cache, &bind.alias.value);
            resolved_aliases.push(alias);
        }

        for alias in &mut resolved_aliases {
            if let Some(typ) = self.create_unifiable_signature(alias.unresolved_type()) {
                *alias.unresolved_type_mut() = typ;
            }
        }

        {
            let mut check =
                KindCheck::new(&self.environment, &self.symbols, self.kind_cache.clone());

            // Setup kind variables for all holes and insert the types in the
            // the type expression into the kindcheck environment
            for alias in &mut resolved_aliases {
                // Create the kind for this binding
                // Test a b : 2 -> 1 -> Type
                // and bind the same variables to the arguments of the type binding
                // ('a' and 'b' in the example)
                let mut id_kind = check.type_kind();
                for generic in alias.unresolved_type_mut().params_mut().iter_mut().rev() {
                    check.instantiate_kinds(&mut generic.kind);
                    id_kind = Kind::function(generic.kind.clone(), id_kind);
                }
                check.add_local(alias.name.clone(), id_kind);
            }

            // Kindcheck all the types in the environment
            for alias in resolved_aliases.iter_mut() {
                check.set_variables(alias.params());
                *alias.unresolved_type_mut() = match **alias.unresolved_type() {
                    Type::Forall(ref args, ref typ, _) => {
                        let mut typ = typ.clone();
                        check.kindcheck_type(&mut typ)?;
                        Type::forall(args.clone(), typ.clone())
                    }
                    _ => {
                        let mut typ = alias.unresolved_type().clone();
                        check.kindcheck_type(&mut typ)?;
                        typ
                    }
                };
            }

            // All kinds are now inferred so replace the kinds store in the AST
            for alias in resolved_aliases.iter_mut() {
                *alias.unresolved_type_mut() = check.finalize_type(alias.unresolved_type().clone());
                for arg in alias.unresolved_type_mut().params_mut() {
                    *arg = check.finalize_generic(arg);
                }
            }
            let alias_group = Alias::group(resolved_aliases);
            for (bind, alias) in bindings.iter_mut().zip(alias_group) {
                bind.finalized_alias = Some(alias);
            }
        }

        // Finally insert the declared types into the global scope
        for bind in bindings {
            if self.environment.stack_types.get(&bind.name.value).is_some() {
                self.errors.push(Spanned {
                    span: expr_check_span(expr),
                    value: TypeError::DuplicateTypeDefinition(bind.name.value.clone()),
                });
            } else {
                self.stack_type(
                    bind.name.value.clone(),
                    &bind.finalized_alias.as_ref().unwrap(),
                );
            }
        }

        Ok(())
    }

    fn kindcheck(&self, typ: &mut ArcType) -> TcResult<()> {
        let mut check = KindCheck::new(&self.environment, &self.symbols, self.kind_cache.clone());
        check.kindcheck_type(typ)?;
        Ok(())
    }

    fn finish_pattern(&mut self, level: u32, pattern: &mut SpannedPattern<Symbol>, typ: &ArcType) {
        match pattern.value {
            Pattern::Ident(ref mut id) => {
                id.typ = typ.clone();
                self.environment
                    .stack
                    .get_mut(&id.name)
                    .expect("ICE: Variable no inserted")
                    .typ = id.typ.clone();
                debug!("{}: {}", self.symbols.string(&id.name), id.typ);
                self.intersect_type(level, &id.name, &id.typ);
            }
            Pattern::Record {
                ref mut typ,
                ref mut fields,
                ..
            } => {
                debug!("{{ .. }}: {}", typ);
                if let Some(finished) = self.finish_type(level, typ) {
                    *typ = finished;
                }
                let record_type = self.remove_alias(typ.clone());
                with_pattern_types(
                    fields,
                    &record_type,
                    |field_name, binding, field_type| match *binding {
                        Some(ref mut pat) => {
                            self.finish_pattern(level, pat, field_type);
                        }
                        None => {
                            self.intersect_type(level, field_name, field_type);
                        }
                    },
                );
            }
            Pattern::Tuple {
                ref typ,
                ref mut elems,
            } => for (elem, field) in elems.iter_mut().zip(typ.row_iter()) {
                self.finish_pattern(level, elem, &field.typ);
            },
            Pattern::Constructor(ref id, ref mut args) => {
                debug!("{}: {}", self.symbols.string(&id.name), typ);
                let len = args.len();
                let iter = args.iter_mut().zip(
                    function_arg_iter(self, typ.clone())
                        .take(len)
                        .collect::<Vec<_>>(),
                );
                for (arg, arg_type) in iter {
                    self.finish_pattern(level, arg, &arg_type);
                }
            }
            Pattern::Error => (),
        }
    }

    fn intersect_type(&mut self, level: u32, symbol: &Symbol, symbol_type: &ArcType) {
        fn no_type_variables(typ: &ArcType) -> bool {
            let mut no_variables = true;
            types::visit_type_opt(typ, &mut |typ: &ArcType| {
                if let Type::Variable(_) = **typ {
                    no_variables = false;
                }
                None
            });
            no_variables
        }
        let typ;
        let mut constraints = FnvMap::default();
        let mut symbol_type = symbol_type.clone();
        self.generalize_type(level, &mut symbol_type);
        {
            let existing_types = self.environment
                .stack
                .get_all(symbol)
                .expect("Symbol is not in scope");
            // Only allow overloading for bindings whose types which do not contain type variables
            // It might be possible to lift this restriction but currently it causes problems
            // which I am not sure how to solve
            if existing_types.len() >= 2 && no_type_variables(&symbol_type) {
                let existing_binding = &existing_types[existing_types.len() - 2];
                debug!(
                    "Intersect `{}`\n{} ∩ {}",
                    symbol,
                    self.subs.real(&existing_binding.typ),
                    self.subs.real(&symbol_type)
                );
                let existing_type = new_skolem_scope(
                    &self.subs,
                    &existing_binding.constraints,
                    &existing_binding.typ,
                );
                let (intersection_constraints, mut result) = {
                    let state = unify_type::State::new(&self.environment, &self.subs);
                    unify::intersection(
                        &self.subs,
                        self.symbols.symbols(),
                        state,
                        &existing_type,
                        &symbol_type.skolemize(&mut FnvMap::default()),
                    )
                };
                constraints = intersection_constraints
                    .into_iter()
                    .map(|((l, r), name)| {
                        let constraints = match *l {
                            Type::Generic(ref gen) => existing_binding.constraints.get(&gen.id),
                            // Since we call `new_skolem_scope` we may find a variable as the constraint
                            // but we really need to return the constraints bound by the generic
                            // instantiating it
                            Type::Variable(ref constraint_var) => match *existing_type {
                                Type::Forall(ref params, _, Some(ref vars)) => vars.iter()
                                    .position(|var| match **var {
                                        Type::Variable(ref var) => var.id == constraint_var.id,
                                        _ => unreachable!(),
                                    })
                                    .and_then(|i| existing_binding.constraints.get(&params[i].id)),
                                _ => None,
                            },
                            _ => None,
                        };
                        (
                            name,
                            Arc::new(
                                constraints
                                    .iter()
                                    .flat_map(|x| x.iter())
                                    .cloned()
                                    .chain(Some(r))
                                    .chain(if constraints.is_none() { Some(l) } else { None })
                                    .collect::<Vec<_>>(),
                            ),
                        )
                    })
                    .collect();

                result = Type::forall(
                    constraints
                        .iter()
                        .map(|constraint| {
                            Generic::new(constraint.0.clone(), constraint.1[0].kind().into_owned())
                        })
                        .collect(),
                    result,
                );
                debug!(
                    "Intersect result {}\n\t{}",
                    result,
                    constraints
                        .iter()
                        .map(|t| format!("{}:{}", t.0, t.1.iter().format("\n\t\t")))
                        .format("\n")
                );
                typ = result
            } else {
                typ = symbol_type.clone()
            }
        }
        let bind = self.environment.stack.get_mut(symbol).unwrap();
        bind.constraints = constraints;
        bind.typ = typ;
    }

    /// Generate a generic variable name which is not used in the current scope
    fn next_variable(&mut self, level: u32, s: &mut String) -> Symbol {
        for c in b'a'..(b'z' + 1) {
            s.push(c as char);
            let symbol = self.symbols.symbol(&s[..]);
            if self.type_variables.get(&symbol).is_none() {
                self.type_variables.insert(
                    symbol.clone(),
                    Type::variable(TypeVariable {
                        id: level,
                        kind: self.kind_cache.typ(),
                    }),
                );
                return symbol;
            }
            s.pop();
        }
        s.push('a');
        self.next_variable(level, s)
    }

    /// Finish a type by replacing all unbound type variables above `level` with generics
    fn finish_type(&mut self, level: u32, typ: &ArcType) -> Option<ArcType> {
        let mut generic = None;
        let mut i = 0;
        self.type_variables.enter_scope();
        let typ = self.finish_type_(level, &mut generic, &mut i, typ);
        let variables = self.type_variables.exit_scope();
        typ.map(|typ| {
            let typ = Type::forall(
                variables
                    .map(|(id, typ)| {
                        Generic {
                            id,
                            kind: typ.kind().into_owned(),
                        }
                    })
                    .collect(),
                typ,
            );
            typ
        })
    }

    fn finish_type_(
        &mut self,
        level: u32,
        generic: &mut Option<String>,
        i: &mut i32,
        typ: &ArcType,
    ) -> Option<ArcType> {
        use base::types::TypeVisitor;

        let mut visitor = types::ControlVisitation(|typ: &ArcType| {
            let replacement = self.subs
                .replace_variable(typ)
                .map(|t| self.finish_type_(level, generic, i, &t).unwrap_or(t));
            let mut typ = typ;
            if let Some(ref t) = replacement {
                debug!("{} ==> {}", typ, t);
                typ = t;
            }
            match **typ {
                Type::Variable(ref var) if self.subs.get_level(var.id) >= level => {
                    if self.subs.get_constraints(var.id).is_some() {
                        let resolved_result = {
                            let state = unify_type::State::new(&self.environment, &self.subs);
                            self.subs.resolve_constraints(|| state.clone(), var, typ)
                        };
                        let resolved_type = match resolved_result {
                            Ok(x) => x.unwrap_or_else(|| typ.clone()),
                            Err(err) => self.error(
                                Span::new(0.into(), 0.into()),
                                TypeError::Unification(
                                    typ.clone(),
                                    typ.clone(),
                                    vec![unify::Error::Substitution(err)],
                                ),
                            ),
                        };
                        Some(resolved_type)
                    } else {
                        // Create a prefix if none exists
                        let id = if generic.is_none() {
                            let mut g = String::new();
                            let symbol = self.next_variable(level, &mut g);
                            *generic = Some(g);
                            symbol
                        } else {
                            let generic = generic.as_ref().unwrap();

                            let generic = format!("{}{}", generic, i);
                            *i += 1;
                            let symbol = self.symbols.symbol(generic);
                            self.type_variables.insert(
                                symbol.clone(),
                                Type::variable(TypeVariable {
                                    id: level,
                                    kind: self.kind_cache.typ(),
                                }),
                            );
                            symbol
                        };
                        let gen: ArcType =
                            Type::generic(Generic::new(id.clone(), var.kind.clone()));
                        self.subs.insert(var.id, gen.clone());
                        Some(gen)
                    }
                }
                Type::ExtendRow {
                    ref types,
                    ref fields,
                    ref rest,
                } => {
                    let new_fields = types::walk_move_types(fields, |field| {
                        // Make a new name base for any unbound variables in the record field
                        // Gives { id : a0 -> a0, const : b0 -> b1 -> b1 }
                        // instead of { id : a0 -> a0, const : a1 -> a2 -> a2 }
                        self.finish_type(level, &field.typ)
                            .map(|typ| Field::new(field.name.clone(), typ))
                    });
                    let new_rest = self.finish_type(level, rest);
                    merge::merge(fields, new_fields, rest, new_rest, |fields, rest| {
                        Type::extend_row(types.clone(), fields, rest)
                    }).or_else(|| replacement.clone())
                }
                _ => {
                    let new_type = types::walk_move_type_opt(typ, &mut |typ: &ArcType| {
                        self.finish_type_(level, generic, i, typ)
                    });
                    match **typ {
                        Type::Forall(ref params, _, _) => for param in params {
                            self.type_variables.remove(&param.id);
                        },
                        Type::Generic(ref generic)
                            if self.type_variables.get(&generic.id).is_none() =>
                        {
                            self.type_variables
                                .insert(generic.id.clone(), self.subs.new_var());
                        }
                        _ => (),
                    }
                    new_type
                        .map(|t| unroll_typ(&t).unwrap_or(t))
                        .or_else(|| replacement.clone())
                }
            }
        });
        visitor.visit(typ)
    }

    fn instantiate_signature(&mut self, typ: &ArcType) -> ArcType {
        let typ = self.instantiate(typ);
        // Put all new generic variable names into scope
        if let Type::Forall(ref params, _, Some(ref vars)) = *typ {
            self.type_variables.extend(
                params
                    .iter()
                    .zip(vars)
                    .map(|(param, var)| (param.id.clone(), var.clone())),
            );
        }
        typ
    }

    // Replaces `Type::Id` types with the actual `Type::Alias` type it refers to
    // Replaces variant names with the actual symbol they should refer to
    // Instantiates Type::Hole with a fresh type variable to ensure the hole only ever refers to a
    // single type variable.
    //
    // Also inserts a `forall` for any implicitly declared variables.
    fn create_unifiable_signature(&mut self, typ: &ArcType) -> Option<ArcType> {
        self.type_variables.enter_scope();
        let result_type = self.create_unifiable_signature_(typ);
        let params = self.type_variables
            .exit_scope()
            .map(|(var, typ)| Generic::new(var, typ.kind().into_owned()))
            .collect::<Vec<_>>();
        if params.is_empty() {
            result_type
        } else {
            Some(Type::forall(
                params,
                result_type.unwrap_or_else(|| typ.clone()),
            ))
        }
    }

    fn create_unifiable_signature_(&mut self, typ: &ArcType) -> Option<ArcType> {
        types::visit_type_opt(
            typ,
            &mut types::ControlVisitation(|typ: &ArcType| {
                match **typ {
                    Type::Ident(ref id) => {
                        // Substitute the Id by its alias if possible
                        let new_id = self.original_symbols.get(id).unwrap_or(id);
                        self.environment
                            .find_type_info(new_id)
                            .map(|alias| alias.clone().into_type())
                            .or_else(|| if id == new_id {
                                None
                            } else {
                                Some(Type::ident(new_id.clone()))
                            })
                    }
                    Type::Variant(ref row) => {
                        let replacement = types::walk_move_type_opt(typ, &mut |typ: &ArcType| {
                            self.create_unifiable_signature_(typ)
                        });
                        let row = replacement.as_ref().unwrap_or(row);
                        let iter = || {
                            row.row_iter()
                                .map(|var| self.original_symbols.get(&var.name))
                        };
                        if iter().any(|opt| opt.is_some()) {
                            // If any of the variants requires a symbol replacement
                            // we create a new type
                            Some(
                                self.type_cache.variant(
                                    iter()
                                        .zip(row.row_iter())
                                        .map(|(new, old)| match new {
                                            Some(new) => Field::new(new.clone(), old.typ.clone()),
                                            None => old.clone(),
                                        })
                                        .collect(),
                                ),
                            )
                        } else {
                            replacement.clone()
                        }
                    }
                    Type::Hole => Some(self.subs.new_var()),
                    Type::ExtendRow {
                        ref types,
                        ref fields,
                        ref rest,
                    } => {
                        let new_fields = types::walk_move_types(fields, |field| {
                            self.create_unifiable_signature(&field.typ)
                                .map(|typ| Field::new(field.name.clone(), typ))
                        });
                        let new_rest = self.create_unifiable_signature_(rest);
                        merge::merge(fields, new_fields, rest, new_rest, |fields, rest| {
                            Type::extend_row(types.clone(), fields, rest)
                        })
                    }
                    Type::Forall(ref params, ref typ, _) => {
                        for param in params {
                            self.type_variables.insert(param.id.clone(), typ.clone());
                        }
                        let result = self.create_unifiable_signature_(typ);
                        // Remove any implicit variables inserted inside the forall since
                        // they were actually bound at this stage
                        for param in params {
                            self.type_variables.remove(&param.id);
                        }
                        result.map(|typ| Type::forall(params.clone(), typ))
                    }
                    Type::Generic(ref generic) => {
                        match self.type_variables.get(&generic.id).cloned() {
                            Some(typ) => Some(typ),
                            None => {
                                let skolem: ArcType = Type::skolem(Skolem {
                                    name: generic.id.clone(),
                                    id: match *self.subs.new_var() {
                                        Type::Variable(ref var) => var.id,
                                        _ => unreachable!(),
                                    },
                                    kind: typ.kind().into_owned(),
                                });
                                self.type_variables
                                    .insert(generic.id.clone(), skolem.clone());
                                Some(skolem)
                            }
                        }
                    }
                    _ => types::walk_move_type_opt(
                        typ,
                        &mut types::ControlVisitation(
                            |typ: &ArcType| self.create_unifiable_signature_(typ),
                        ),
                    ),
                }
            }),
        )
    }

    fn merge_signature(
        &mut self,
        span: Span<BytePos>,
        level: u32,
        expected: &ArcType,
        mut actual: ArcType,
    ) -> ArcType {
        debug!("Merge {} : {}", expected, actual);
        let expected = self.skolemize(&expected);
        let state = unify_type::State::new(&self.environment, &self.subs);
        match unify_type::merge_signature(
            &self.subs,
            &mut self.type_variables,
            level,
            state,
            &expected,
            &actual,
        ) {
            Ok(typ) => self.subs.set_type(typ),
            Err(errors) => {
                let mut expected = expected.clone();
                expected = self.subs.set_type(expected);
                actual = self.subs.set_type(actual);
                debug!(
                    "Error '{:?}' between:\n>> {}\n>> {}",
                    errors,
                    expected,
                    actual
                );
                let err = TypeError::Unification(expected, actual, apply_subs(&self.subs, errors));
                self.errors.push(Spanned {
                    span: span,
                    value: err,
                });
                self.subs.new_var()
            }
        }
    }

    fn unify_span(&mut self, span: Span<BytePos>, expected: &ArcType, actual: ArcType) -> ArcType {
        match self.unify(expected, actual) {
            Ok(typ) => typ,
            Err(err) => {
                self.errors.push(Spanned {
                    span: span,
                    value: err,
                });
                self.subs.new_var()
            }
        }
    }

    fn unify(&self, expected: &ArcType, mut actual: ArcType) -> TcResult<ArcType> {
        debug!("Unify start {} <=> {}", expected, actual);
        let state = unify_type::State::new(&self.environment, &self.subs);
        match unify::unify(&self.subs, state, expected, &actual) {
            Ok(typ) => Ok(self.subs.set_type(typ)),
            Err(errors) => {
                let mut expected = expected.clone();
                expected = self.subs.set_type(expected);
                actual = self.subs.set_type(actual);
                debug!(
                    "Error '{:?}' between:\n>> {}\n>> {}",
                    errors,
                    expected,
                    actual
                );
                Err(TypeError::Unification(
                    expected,
                    actual,
                    apply_subs(&self.subs, errors),
                ))
            }
        }
    }

    fn remove_alias(&self, typ: ArcType) -> ArcType {
        resolve::remove_alias(&self.environment, &typ)
            .unwrap_or(None)
            .unwrap_or(typ)
    }

    fn remove_aliases(&self, typ: ArcType) -> ArcType {
        resolve::remove_aliases(&self.environment, typ)
    }

    fn skolemize(&mut self, typ: &ArcType) -> ArcType {
        self.named_variables.clear();
        self.named_variables.extend(
            self.type_variables
                .iter()
                .map(|(k, v)| (k.clone(), v.clone())),
        );
        typ.skolemize(&mut self.named_variables)
    }

    fn instantiate_generics(&mut self, typ: &ArcType) -> ArcType {
        self.named_variables.clear();
        typ.instantiate_generics(&mut self.named_variables)
    }

    fn instantiate(&mut self, typ: &ArcType) -> ArcType {
        self.instantiate_constrained(&FnvMap::default(), typ)
    }

    fn instantiate_constrained(
        &mut self,
        constraints: &FnvMap<Symbol, Constraints<ArcType>>,
        typ: &ArcType,
    ) -> ArcType {
        new_skolem_scope(&self.subs, constraints, typ)
    }

    fn error_on_duplicated_field(
        &mut self,
        duplicated_fields: &mut FnvSet<String>,
        new_name: Spanned<Symbol, BytePos>,
    ) -> bool {
        let span = new_name.span;
        duplicated_fields
            .replace(new_name.value.declared_name().to_string())
            .map_or(true, |name| {
                self.errors.push(Spanned {
                    span: span,
                    value: TypeError::DuplicateField(name),
                });
                false
            })
    }
}

fn with_pattern_types<F>(
    fields: &mut [PatternField<Symbol, SpannedPattern<Symbol>>],
    typ: &ArcType,
    mut f: F,
) where
    F: FnMut(&Symbol, &mut Option<SpannedPattern<Symbol>>, &ArcType),
{
    for field in fields {
        // If the field in the pattern does not exist (undefined field error) then skip it as
        // the error itself will already have been reported
        let opt = typ.row_iter()
            .find(|type_field| type_field.name.name_eq(&field.name.value));
        if let Some(associated_type) = opt {
            f(&field.name.value, &mut field.value, &associated_type.typ);
        }
    }
}

fn apply_subs(
    subs: &Substitution<ArcType>,
    errors: Errors<UnifyTypeError<Symbol>>,
) -> Vec<UnifyTypeError<Symbol>> {
    use unify::Error::*;
    errors
        .into_iter()
        .map(|error| match error {
            TypeMismatch(expected, actual) => {
                TypeMismatch(subs.set_type(expected), subs.set_type(actual))
            }
            Substitution(err) => Substitution(match err {
                substitution::Error::Occurs(var, typ) => {
                    substitution::Error::Occurs(var, subs.set_type(typ))
                }
                substitution::Error::Constraint(typ, mut constraints) => {
                    for typ in Arc::make_mut(&mut constraints) {
                        *typ = subs.set_type(typ.clone());
                    }
                    substitution::Error::Constraint(subs.set_type(typ), constraints)
                }
            }),
            Other(err) => Other(err),
        })
        .collect()
}

pub fn extract_generics(args: &[ArcType]) -> Vec<Generic<Symbol>> {
    args.iter()
        .map(|arg| match **arg {
            Type::Generic(ref gen) => gen.clone(),
            _ => panic!("The type on the lhs of a type binding did not have all generic arguments"),
        })
        .collect()
}

fn get_alias_app<'a>(
    env: &'a TypeEnv,
    typ: &'a ArcType,
) -> Option<(&'a AliasData<Symbol, ArcType>, &'a [ArcType])> {
    match **typ {
        Type::Alias(ref alias) => Some((alias, &[][..])),
        Type::App(ref alias, ref args) => match **alias {
            Type::Alias(ref alias) => Some((alias, args)),
            _ => None,
        },
        _ => typ.alias_ident().and_then(|id| {
            env.find_type_info(id)
                .map(|alias| (&**alias, typ.unapplied_args()))
        }),
    }
}

struct FunctionArgIter<'a, 'b: 'a> {
    tc: &'a mut Typecheck<'b>,
    typ: ArcType,
}

impl<'a, 'b> Iterator for FunctionArgIter<'a, 'b> {
    type Item = ArcType;
    fn next(&mut self) -> Option<ArcType> {
        loop {
            self.typ = self.tc.instantiate_generics(&self.typ);
            let (arg, new) = match self.typ.as_function() {
                Some((arg, ret)) => (Some(arg.clone()), ret.clone()),
                None => match get_alias_app(&self.tc.environment, &self.typ) {
                    Some((alias, args)) => match alias.unresolved_type().apply_args(args) {
                        Some(typ) => (None, typ.clone()),
                        None => return None,
                    },
                    None => return Some(self.tc.subs.new_var()),
                },
            };
            self.typ = new;
            if let Some(arg) = arg {
                return Some(arg);
            }
        }
    }
}

fn function_arg_iter<'a, 'b>(tc: &'a mut Typecheck<'b>, typ: ArcType) -> FunctionArgIter<'a, 'b> {
    FunctionArgIter { tc: tc, typ: typ }
}

/// Returns a span of the innermost expression of a group of nested `let` and `type` bindings.
/// This span is useful for more precisely marking the span of a type error.
///
/// ```ignore
/// let x: Int =
///     let y = 1.0
///     ~~~~~~~~~~~
///     y
///     ~
///     ^
/// x
/// ```
fn expr_check_span(e: &SpannedExpr<Symbol>) -> Span<BytePos> {
    match e.value {
        Expr::LetBindings(_, ref b) | Expr::TypeBindings(_, ref b) => expr_check_span(b),
        _ => e.span,
    }
}

/// Removes layers of `Type::App` and `Type::Record` by packing them into a single `Type::App` or
/// `Type::Record`
///
/// Example:
///
/// ```rust
/// #[macro_use]
/// extern crate collect_mac;
/// extern crate gluon_base;
/// extern crate gluon_check;
///
/// use gluon_base::types::{Type, ArcType, BuiltinType};
/// use gluon_check::typecheck::unroll_typ;
///
/// # fn main() {
/// let i: ArcType = Type::int();
/// let s: ArcType = Type::string();
/// assert_eq!(unroll_typ(&Type::app(Type::app(i.clone(), collect![s.clone()]), collect![i.clone()])),
///            Some(Type::app(i.clone(), collect![s.clone(), i.clone()])));
/// assert_eq!(unroll_typ(&Type::app(Type::app(i.clone(), collect![i.clone()]), collect![s.clone()])),
///            Some(Type::app(i.clone(), collect![i.clone(), s.clone()])));
/// let f: ArcType = Type::builtin(BuiltinType::Function);
/// assert_eq!(unroll_typ(&Type::app(Type::app(f.clone(), collect![i.clone()]), collect![s.clone()])),
///            Some(Type::function(collect![i.clone()], s.clone())));
/// # }
/// ```
pub fn unroll_typ(typ: &ArcType) -> Option<ArcType> {
    let mut args = AppVec::new();
    let mut current = match **typ {
        Type::App(ref l, ref rest) => {
            // No need to unroll if `l` is not an application as that will just result in returning
            // an application that is identical to `typ`
            match **l {
                Type::App(..) => (),
                _ => return None,
            }
            args.extend(rest.iter().rev().cloned());
            l
        }
        _ => return unroll_record(typ),
    };
    while let Type::App(ref l, ref rest) = **current {
        args.extend(rest.iter().rev().cloned());
        current = l;
    }
    if args.is_empty() {
        None
    } else {
        args.reverse();
        Some(Type::app(current.clone(), args))
    }
}

fn unroll_record(typ: &Type<Symbol>) -> Option<ArcType> {
    let mut new_types = Vec::new();
    let mut new_fields = Vec::new();
    let mut current = match *typ {
        Type::ExtendRow {
            ref types,
            ref fields,
            ref rest,
        } => match **rest {
            Type::ExtendRow { .. } => {
                new_types.extend_from_slice(types);
                new_fields.extend_from_slice(fields);
                rest
            }
            _ => return None,
        },
        _ => return None,
    };
    while let Type::ExtendRow {
        ref types,
        ref fields,
        ref rest,
    } = **current
    {
        new_types.extend_from_slice(types);
        new_fields.extend_from_slice(fields);
        current = rest;
    }
    if new_types.is_empty() && new_fields.is_empty() {
        None
    } else {
        Some(Type::extend_row(new_types, new_fields, current.clone()))
    }
}
