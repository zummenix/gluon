use std::borrow::Cow;

use fnv::FnvMap;
use types::{AliasData, AliasRef, ArcType, Type, TypeEnv};
use symbol::Symbol;

quick_error! {
    #[derive(Debug)]
    pub enum Error {
        UndefinedType(id: Symbol) {
            description("undefined type")
            display("Type `{}` does not exist.", id)
        }
    }
}

/// Removes type aliases from `typ` until it is an actual type
pub fn remove_aliases(env: &TypeEnv, mut typ: ArcType) -> ArcType {
    while let Ok(Some(new)) = remove_alias(env, &typ) {
        typ = new;
    }
    typ
}

pub fn remove_aliases_cow<'t>(env: &TypeEnv, typ: &'t ArcType) -> Cow<'t, ArcType> {
    match remove_alias(env, typ) {
        Ok(Some(typ)) => Cow::Owned(remove_aliases(env, typ)),
        _ => Cow::Borrowed(typ),
    }
}

pub fn canonical_alias<'t, F>(env: &TypeEnv, typ: &'t ArcType, canonical: F) -> Cow<'t, ArcType>
where
    F: Fn(&AliasData<Symbol, ArcType>) -> bool,
{
    match peek_alias(env, typ) {
        Ok(Some(alias)) if !canonical(alias) => alias
            .typ()
            .apply_args(&typ.unapplied_args())
            .map(|typ| {
                Cow::Owned(canonical_alias(env, &typ, canonical).into_owned())
            })
            .unwrap_or(Cow::Borrowed(typ)),
        _ => Cow::Borrowed(typ),
    }
}

/// Expand `typ` if it is an alias that can be expanded and return the expanded type.
/// Returns `None` if the type is not an alias or the alias could not be expanded.
pub fn remove_alias(env: &TypeEnv, typ: &ArcType) -> Result<Option<ArcType>, Error> {
    let typ = typ.skolemize(&mut FnvMap::default());
    Ok(peek_alias(env, &typ)?.and_then(|alias| {
        // Opaque types should only exist as the alias itself
        if **alias.unresolved_type().remove_forall() == Type::Opaque {
            return None;
        }
        alias.typ().apply_args(&typ.unapplied_args())
    }))
}

pub fn peek_alias<'t>(
    env: &'t TypeEnv,
    typ: &'t ArcType,
) -> Result<Option<&'t AliasRef<Symbol, ArcType>>, Error> {
    fn extract_alias(
        typ: &ArcType,
        given_arguments_count: usize,
    ) -> Option<&AliasRef<Symbol, ArcType>> {
        match **typ {
            Type::Alias(ref alias) if alias.params().len() == given_arguments_count => Some(alias),
            Type::App(ref alias, ref args) => {
                extract_alias(alias, args.len() + given_arguments_count)
            }
            _ => None,
        }
    }

    let maybe_alias = extract_alias(typ, 0);

    match typ.alias_ident() {
        Some(id) => {
            let alias = match maybe_alias {
                Some(alias) => alias,
                None => env.find_type_info(id)
                    .ok_or_else(|| Error::UndefinedType(id.clone()))?,
            };
            Ok(Some(alias))
        }
        None => Ok(None),
    }
}
