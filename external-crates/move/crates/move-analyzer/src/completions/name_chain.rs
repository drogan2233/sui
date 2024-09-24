// Copyright (c) The Move Contributors
// SPDX-License-Identifier: Apache-2.0

// Auto-completions for name chains, such as `mod::struct::field` or `mod::function`,
// both in the code (e.g., types) and in `use` statements.

use crate::{
    completions::utils::{
        call_completion_item, completion_item, mod_defs, PRIMITIVE_TYPE_COMPLETIONS,
    },
    symbols::{
        expansion_mod_ident_to_map_key, ChainCompletionKind, ChainInfo, CursorContext, DefInfo,
        FunType, MemberDef, MemberDefInfo, Symbols, VariantInfo,
    },
};
use itertools::Itertools;
use lsp_types::{CompletionItem, CompletionItemKind, InsertTextFormat};
use move_compiler::{
    expansion::ast::{Address, ModuleIdent, ModuleIdent_, Visibility},
    parser::ast as P,
    shared::{ide::AliasAutocompleteInfo, Identifier, Name, NumericalAddress},
};
use move_ir_types::location::{sp, Loc};
use move_symbol_pool::Symbol;
use std::collections::BTreeSet;

/// Describes kind of the name access chain component.
enum ChainComponentKind {
    Package(P::LeadingNameAccess),
    Module(ModuleIdent),
    Member(ModuleIdent, Symbol),
}

/// Information about access chain component - its location and kind.
struct ChainComponentInfo {
    loc: Loc,
    kind: ChainComponentKind,
}

impl ChainComponentInfo {
    fn new(loc: Loc, kind: ChainComponentKind) -> Self {
        Self { loc, kind }
    }
}

/// Handle name chain auto-completion at a given position. The gist of this approach is to first
/// identify what the first component of the access chain represents (as it may be a package, module
/// or a member) and if the chain has other components, recursively process them in turn to either
/// - finish auto-completion if cursor is on a given component's identifier
/// - identify what the subsequent component represents and keep going
pub fn name_chain_completions(
    symbols: &Symbols,
    cursor: &CursorContext,
    colon_colon_triggered: bool,
) -> (Vec<CompletionItem>, bool) {
    eprintln!("looking for name access chains");
    let mut completions = vec![];
    let mut completion_finalized = false;
    let Some(ChainInfo {
        chain,
        kind: chain_kind,
        inside_use,
    }) = cursor.find_access_chain()
    else {
        eprintln!("no access chain");
        return (completions, completion_finalized);
    };

    let (leading_name, path_entries) = match &chain.value {
        P::NameAccessChain_::Single(entry) => (
            sp(entry.name.loc, P::LeadingNameAccess_::Name(entry.name)),
            vec![],
        ),
        P::NameAccessChain_::Path(name_path) => (
            name_path.root.name,
            name_path.entries.iter().map(|e| e.name).collect::<Vec<_>>(),
        ),
    };

    // there may be access chains for which there is not auto-completion info generated by the
    // compiler but which still have to be handled (e.g., chains starting with numeric address)
    let info = symbols
        .compiler_info
        .path_autocomplete_info
        .get(&leading_name.loc)
        .cloned()
        .unwrap_or_else(AliasAutocompleteInfo::new);

    eprintln!("found access chain for auto-completion (adddreses: {}, modules: {}, members: {}, tparams: {}",
              info.addresses.len(), info.modules.len(), info.members.len(), info.type_params.len());

    // if we are auto-completing for an access chain, there is no need to include default completions
    completion_finalized = true;

    if leading_name.loc.contains(&cursor.loc) {
        // at first position of the chain suggest all packages that are available regardless of what
        // the leading name represents, as a package always fits at that position, for example:
        // OxCAFE::...
        // some_name::...
        // ::some_name
        //
        completions.extend(
            all_packages(symbols, &info)
                .iter()
                .map(|n| completion_item(n.as_str(), CompletionItemKind::UNIT)),
        );

        // only if leading name is actually a name, modules or module members are a correct
        // auto-completion in the first position
        if let P::LeadingNameAccess_::Name(_) = &leading_name.value {
            completions.extend(
                info.modules
                    .keys()
                    .map(|n| completion_item(n.as_str(), CompletionItemKind::MODULE)),
            );
            completions.extend(all_single_name_member_completions(
                symbols,
                cursor,
                &info.members,
                chain_kind,
            ));
            if matches!(chain_kind, ChainCompletionKind::Type) {
                completions.extend(PRIMITIVE_TYPE_COMPLETIONS.clone());
                completions.extend(
                    info.type_params
                        .iter()
                        .map(|t| completion_item(t.as_str(), CompletionItemKind::TYPE_PARAMETER)),
                );
            }
        }
    } else if let Some(next_kind) = first_name_chain_component_kind(symbols, &info, leading_name) {
        completions_for_name_chain_entry(
            symbols,
            cursor,
            &info,
            ChainComponentInfo::new(leading_name.loc, next_kind),
            chain_kind,
            &path_entries,
            /* path_index */ 0,
            colon_colon_triggered,
            inside_use,
            &mut completions,
        );
    }

    eprintln!("found {} access chain completions", completions.len());

    (completions, completion_finalized)
}

/// Handles auto-completions for "regular" `use` declarations (name access chains in `use fun`
/// declarations are handled as part of name chain completions).
pub fn use_decl_completions(
    symbols: &Symbols,
    cursor: &CursorContext,
) -> (Vec<CompletionItem>, bool) {
    eprintln!("looking for use declarations");
    let mut completions = vec![];
    let mut completion_finalized = false;
    let Some(use_) = cursor.find_use_decl() else {
        eprintln!("no use declaration");
        return (completions, completion_finalized);
    };
    eprintln!("use declaration {:?}", use_);

    // if we are auto-completing for a use decl, there is no need to include default completions
    completion_finalized = true;

    // there is no auto-completion info generated by the compiler for this but helper methods used
    // here are shared with name chain completion where it may exist, so we create an "empty" one
    // here
    let info = AliasAutocompleteInfo::new();

    match use_ {
        P::Use::ModuleUse(sp!(_, mod_ident), mod_use) => {
            if mod_ident.address.loc.contains(&cursor.loc) {
                // cursor on package (e.g., on `some_pkg` in `some_pkg::some_mod`)
                completions.extend(
                    all_packages(symbols, &info)
                        .iter()
                        .map(|n| completion_item(n.as_str(), CompletionItemKind::UNIT)),
                );
            } else if cursor.loc.start() > mod_ident.address.loc.end()
                && cursor.loc.end() <= mod_ident.module.loc().end()
            {
                // cursor is either at the `::` succeeding package/address or at the identifier
                // following that particular `::`
                for ident in pkg_mod_identifiers(symbols, &info, &mod_ident.address) {
                    completions.push(completion_item(
                        ident.value.module.value().as_str(),
                        CompletionItemKind::MODULE,
                    ));
                }
            } else {
                completions.extend(module_use_completions(
                    symbols,
                    cursor,
                    &info,
                    &mod_use,
                    &mod_ident.address,
                    &mod_ident.module,
                ));
            }
        }
        P::Use::NestedModuleUses(leading_name, uses) => {
            if leading_name.loc.contains(&cursor.loc) {
                // cursor on package
                completions.extend(
                    all_packages(symbols, &info)
                        .iter()
                        .map(|n| completion_item(n.as_str(), CompletionItemKind::UNIT)),
                );
            } else {
                if let Some((first_name, _)) = uses.first() {
                    if cursor.loc.start() > leading_name.loc.end()
                        && cursor.loc.end() <= first_name.loc().start()
                    {
                        // cursor is after `::` succeeding address/package but before the first
                        // module
                        for ident in pkg_mod_identifiers(symbols, &info, &leading_name) {
                            completions.push(completion_item(
                                ident.value.module.value().as_str(),
                                CompletionItemKind::MODULE,
                            ));
                        }
                        // no point in falling through to the uses loop below
                        return (completions, completion_finalized);
                    }
                }

                for (mod_name, mod_use) in &uses {
                    if mod_name.loc().contains(&cursor.loc) {
                        for ident in pkg_mod_identifiers(symbols, &info, &leading_name) {
                            completions.push(completion_item(
                                ident.value.module.value().as_str(),
                                CompletionItemKind::MODULE,
                            ));
                        }
                        // no point checking other locations
                        break;
                    }
                    completions.extend(module_use_completions(
                        symbols,
                        cursor,
                        &info,
                        mod_use,
                        &leading_name,
                        mod_name,
                    ));
                }
            }
        }
        P::Use::Fun { .. } => (), // already handled as part of name chain completion
        P::Use::Partial {
            package,
            colon_colon,
            opening_brace: _,
        } => {
            if package.loc.contains(&cursor.loc) {
                // cursor on package name/address
                completions.extend(
                    all_packages(symbols, &info)
                        .iter()
                        .map(|n| completion_item(n.as_str(), CompletionItemKind::UNIT)),
                );
            }
            if let Some(colon_colon_loc) = colon_colon {
                if cursor.loc.start() >= colon_colon_loc.start() {
                    // cursor is on or past `::`
                    for ident in pkg_mod_identifiers(symbols, &info, &package) {
                        completions.push(completion_item(
                            ident.value.module.value().as_str(),
                            CompletionItemKind::MODULE,
                        ));
                    }
                }
            }
        }
    }

    (completions, completion_finalized)
}

/// Handles auto-completion for structs and enums variants, including fields contained
/// by the struct or variant.
fn datatype_completion(
    cursor: &CursorContext,
    defining_mod_ident: &ModuleIdent_,
    field_container: Symbol,
    kind: CompletionItemKind,
    field_names: &[Name],
    named_fields: bool,
) -> Vec<CompletionItem> {
    // always add a completion for the datatype itself (for type completion)
    let mut completions = vec![completion_item(&field_container, kind)];

    let defining_mod_ident_str = expansion_mod_ident_to_map_key(defining_mod_ident);
    let current_mod_ident_str =
        expansion_mod_ident_to_map_key(&cursor.module.as_ref().unwrap().value);

    // only add fields if there are some and we are in the same module as the datatype
    if field_names.is_empty() || defining_mod_ident_str != current_mod_ident_str {
        return completions;
    }

    // fields on separate lines if there is more than two and if they are named
    let separator = if field_names.len() > 2 && named_fields {
        ",\n\t"
    } else {
        ", "
    };
    let fields_list = field_names
        .iter()
        .enumerate()
        .map(|(idx, name)| {
            if named_fields {
                format!("${{{}:{}}}", idx + 1, name)
            } else {
                format!("${{{}}}", idx + 1)
            }
        })
        .collect::<Vec<_>>()
        .join(separator);

    let (label, insert_text) = if !named_fields {
        (
            format!("{field_container}(..)"),
            // positional fields always on the same line
            format!("{field_container}({fields_list})"),
        )
    } else if field_names.len() > 2 {
        (
            format!("{field_container}{{..}}"),
            // more than two named fields, each on a separate line
            format!("{field_container} {{\n\t{fields_list},\n}}"),
        )
    } else {
        (
            format!("{field_container}{{..}}"),
            // fewer than three named fields, all on the same line
            format!("{field_container} {{ {fields_list} }}"),
        )
    };
    let field_completion = CompletionItem {
        label,
        kind: Some(kind),
        insert_text: Some(insert_text),
        insert_text_format: Some(InsertTextFormat::SNIPPET),
        ..Default::default()
    };
    completions.push(field_completion);
    completions
}

/// Returns all possible completions for a module member (e.g., a datatype) component of a name
/// access chain, where the prefix of this component (e.g, in `some_pkg::some_mod::`) represents a
/// module specified in `prefix_mod_ident`. The `inside_use` parameter determines if completion is
/// for "regular" access chain or for completion within a `use` statement.
fn module_member_completions(
    symbols: &Symbols,
    cursor: &CursorContext,
    prefix_mod_ident: &ModuleIdent,
    chain_kind: ChainCompletionKind,
    inside_use: bool,
) -> Vec<CompletionItem> {
    use ChainCompletionKind as CT;

    let mut completions = vec![];

    let Some(mod_defs) = mod_defs(symbols, &prefix_mod_ident.value) else {
        return completions;
    };

    // list all members or only publicly visible ones
    let mut same_module = false;
    let mut same_package = false;
    if let Some(cursor_mod_ident) = cursor.module {
        if &cursor_mod_ident == prefix_mod_ident {
            same_module = true;
        }
        if cursor_mod_ident.value.address == prefix_mod_ident.value.address {
            same_package = true;
        }
    }

    if matches!(chain_kind, CT::Function) || matches!(chain_kind, CT::All) {
        let fun_completions = mod_defs
            .functions
            .iter()
            .filter_map(|(fname, fdef)| {
                symbols
                    .def_info(&fdef.name_loc)
                    .map(|def_info| (fname, def_info))
            })
            .filter(|(_, def_info)| {
                if let DefInfo::Function(_, visibility, ..) = def_info {
                    match visibility {
                        Visibility::Internal => same_module,
                        Visibility::Package(_) => same_package,
                        _ => true,
                    }
                } else {
                    false
                }
            })
            .filter_map(|(fname, def_info)| {
                if let DefInfo::Function(
                    _,
                    _,
                    fun_type,
                    _,
                    type_args,
                    arg_names,
                    arg_types,
                    ret_type,
                    _,
                ) = def_info
                {
                    Some(call_completion_item(
                        &prefix_mod_ident.value,
                        matches!(fun_type, FunType::Macro),
                        None,
                        fname,
                        type_args,
                        arg_names,
                        arg_types,
                        ret_type,
                        inside_use,
                    ))
                } else {
                    None
                }
            });
        completions.extend(fun_completions);
    }

    if matches!(chain_kind, CT::Type) || matches!(chain_kind, CT::All) {
        completions.extend(mod_defs.structs.iter().flat_map(|(sname, member_def)| {
            struct_completion(cursor, &mod_defs.ident, *sname, member_def)
        }));
        completions.extend(
            mod_defs
                .enums
                .keys()
                .map(|ename| completion_item(ename, CompletionItemKind::ENUM)),
        );
    }

    if matches!(chain_kind, CT::All) && same_module {
        completions.extend(
            mod_defs
                .constants
                .keys()
                .map(|cname| completion_item(cname, CompletionItemKind::CONSTANT)),
        );
    }

    completions
}

/// Computes completions for a struct.
fn struct_completion(
    cursor: &CursorContext,
    defining_mod_ident: &ModuleIdent_,
    name: Symbol,
    member_def: &MemberDef,
) -> Vec<CompletionItem> {
    let MemberDef {
        info: MemberDefInfo::Struct {
            field_defs,
            positional,
        },
        ..
    } = member_def
    else {
        return vec![completion_item(&name, CompletionItemKind::STRUCT)];
    };
    datatype_completion(
        cursor,
        defining_mod_ident,
        name,
        CompletionItemKind::STRUCT,
        &field_defs.iter().map(|d| sp(d.loc, d.name)).collect_vec(),
        !positional,
    )
}

/// Returns completion item if a given name/alias identifies a valid member of a given module
/// available in the completion scope as if it was a single-length name chain.
fn single_name_member_completion(
    symbols: &Symbols,
    cursor: &CursorContext,
    mod_ident: &ModuleIdent_,
    member_alias: &Symbol,
    member_name: &Symbol,
    chain_kind: ChainCompletionKind,
) -> Vec<CompletionItem> {
    use ChainCompletionKind as CT;

    let Some(mod_defs) = mod_defs(symbols, mod_ident) else {
        return vec![];
    };

    // is it a function?
    if let Some(fdef) = mod_defs.functions.get(member_name) {
        if !(matches!(chain_kind, CT::Function) || matches!(chain_kind, CT::All)) {
            return vec![];
        }
        let Some(DefInfo::Function(.., fun_type, _, type_args, arg_names, arg_types, ret_type, _)) =
            symbols.def_info(&fdef.name_loc)
        else {
            return vec![];
        };
        return vec![call_completion_item(
            mod_ident,
            matches!(fun_type, FunType::Macro),
            None,
            member_alias,
            type_args,
            arg_names,
            arg_types,
            ret_type,
            /* inside_use */ false,
        )];
    };

    // is it a struct?
    if let Some(member_def) = mod_defs.structs.get(member_name) {
        if !(matches!(chain_kind, CT::Type) || matches!(chain_kind, CT::All)) {
            return vec![];
        }
        return struct_completion(cursor, &mod_defs.ident, *member_alias, member_def);
    }

    // is it an enum?
    if mod_defs.enums.get(member_name).is_some() {
        if !(matches!(chain_kind, CT::Type) || matches!(chain_kind, CT::All)) {
            return vec![];
        }
        return vec![completion_item(
            member_alias.as_str(),
            CompletionItemKind::ENUM,
        )];
    }

    // is it a const?
    if mod_defs.constants.get(member_name).is_some() {
        if !matches!(chain_kind, CT::All) {
            return vec![];
        }
        return vec![completion_item(
            member_alias.as_str(),
            CompletionItemKind::CONSTANT,
        )];
    }

    vec![]
}

/// Returns completion items for all members of a given module as if they were single-length name
/// chains.
fn all_single_name_member_completions(
    symbols: &Symbols,
    cursor: &CursorContext,
    members_info: &BTreeSet<(Symbol, ModuleIdent, Name)>,
    chain_kind: ChainCompletionKind,
) -> Vec<CompletionItem> {
    let mut completions = vec![];
    for (member_alias, sp!(_, mod_ident), member_name) in members_info {
        let member_completions = single_name_member_completion(
            symbols,
            cursor,
            mod_ident,
            member_alias,
            &member_name.value,
            chain_kind,
        );
        completions.extend(member_completions);
    }
    completions
}

/// Checks if a given module identifier represents a module in a package identifier by
/// `leading_name`.
fn is_pkg_mod_ident(mod_ident: &ModuleIdent_, leading_name: &P::LeadingNameAccess) -> bool {
    match mod_ident.address {
        Address::NamedUnassigned(name) => matches!(leading_name.value,
            P::LeadingNameAccess_::Name(n) | P::LeadingNameAccess_::GlobalAddress(n) if name == n),
        Address::Numerical {
            name,
            value,
            name_conflict: _,
        } => match leading_name.value {
            P::LeadingNameAccess_::AnonymousAddress(addr) if addr == value.value => true,
            P::LeadingNameAccess_::Name(addr_name)
            | P::LeadingNameAccess_::GlobalAddress(addr_name)
                if Some(addr_name) == name =>
            {
                true
            }
            _ => false,
        },
    }
}

/// Gets module identifiers for a given package identified by `leading_name`.
fn pkg_mod_identifiers(
    symbols: &Symbols,
    info: &AliasAutocompleteInfo,
    leading_name: &P::LeadingNameAccess,
) -> BTreeSet<ModuleIdent> {
    info.modules
        .values()
        .filter(|mod_ident| is_pkg_mod_ident(&mod_ident.value, leading_name))
        .copied()
        .chain(
            symbols
                .file_mods
                .values()
                .flatten()
                .map(|mdef| sp(mdef.name_loc, mdef.ident))
                .filter(|mod_ident| is_pkg_mod_ident(&mod_ident.value, leading_name)),
        )
        .collect::<BTreeSet<_>>()
}

/// Computes completions for a single enum variant.
fn variant_completion(
    symbols: &Symbols,
    cursor: &CursorContext,
    defining_mod_ident: &ModuleIdent_,
    vinfo: &VariantInfo,
) -> Vec<CompletionItem> {
    let Some(DefInfo::Variant(_, _, _, positional, field_names, ..)) =
        symbols.def_info.get(&vinfo.name.loc)
    else {
        return vec![completion_item(
            vinfo.name.value.as_str(),
            CompletionItemKind::ENUM_MEMBER,
        )];
    };

    datatype_completion(
        cursor,
        defining_mod_ident,
        vinfo.name.value,
        CompletionItemKind::ENUM_MEMBER,
        field_names,
        !positional,
    )
}

/// Computes completions for variants of a given enum.
fn all_variant_completions(
    symbols: &Symbols,
    cursor: &CursorContext,
    mod_ident: &ModuleIdent,
    datatype_name: Symbol,
) -> Vec<CompletionItem> {
    let Some(mod_defs) = mod_defs(symbols, &mod_ident.value) else {
        return vec![];
    };

    let Some(edef) = mod_defs.enums.get(&datatype_name) else {
        return vec![];
    };

    let Some(DefInfo::Enum(.., variants, _)) = symbols.def_info.get(&edef.name_loc) else {
        return vec![];
    };

    variants
        .iter()
        .flat_map(|vinfo| variant_completion(symbols, cursor, &mod_defs.ident, vinfo))
        .collect_vec()
}

/// Computes completions for a given chain entry: `prev_kind` determines the kind of previous chain
/// component, and `chain_kind` contains information about the entity that the whole chain may
/// represent (e.g., a type of or a function).
fn name_chain_entry_completions(
    symbols: &Symbols,
    cursor: &CursorContext,
    info: &AliasAutocompleteInfo,
    prev_kind: ChainComponentKind,
    chain_kind: ChainCompletionKind,
    inside_use: bool,
    completions: &mut Vec<CompletionItem>,
) {
    match prev_kind {
        ChainComponentKind::Package(leading_name) => {
            for mod_ident in pkg_mod_identifiers(symbols, info, &leading_name) {
                completions.push(completion_item(
                    mod_ident.value.module.value().as_str(),
                    CompletionItemKind::MODULE,
                ));
            }
        }
        ChainComponentKind::Module(mod_ident) => {
            completions.extend(module_member_completions(
                symbols, cursor, &mod_ident, chain_kind, inside_use,
            ));
        }
        ChainComponentKind::Member(mod_ident, member_name) => {
            completions.extend(all_variant_completions(
                symbols,
                cursor,
                &mod_ident,
                member_name,
            ));
        }
    }
}

/// Computes the kind of the next chain component (based on what the previous one, represented by
/// `prev_kind` was).
fn next_name_chain_component_kind(
    symbols: &Symbols,
    info: &AliasAutocompleteInfo,
    prev_kind: ChainComponentKind,
    component_name: Name,
) -> Option<ChainComponentKind> {
    match prev_kind {
        ChainComponentKind::Package(leading_name) => {
            pkg_mod_identifiers(symbols, info, &leading_name)
                .into_iter()
                .find(|mod_ident| mod_ident.value.module.value() == component_name.value)
                .map(ChainComponentKind::Module)
        }
        ChainComponentKind::Module(mod_ident) => {
            Some(ChainComponentKind::Member(mod_ident, component_name.value))
        }
        ChainComponentKind::Member(_, _) => None, // no more "after" completions to be processed
    }
}

/// Walks down a name chain, looking for the relevant portion that contains the cursor. When it
/// finds, it calls to `name_chain_entry_completions` to compute and return the completions.
fn completions_for_name_chain_entry(
    symbols: &Symbols,
    cursor: &CursorContext,
    info: &AliasAutocompleteInfo,
    prev_info: ChainComponentInfo,
    chain_kind: ChainCompletionKind,
    path_entries: &[Name],
    path_index: usize,
    colon_colon_triggered: bool,
    inside_use: bool,
    completions: &mut Vec<CompletionItem>,
) {
    let ChainComponentInfo {
        loc: prev_loc,
        kind: prev_kind,
    } = prev_info;

    let mut at_colon_colon = false;
    if path_index == path_entries.len() {
        // the only reason we would not return here is if we were at `::` which is past the location
        // of the last path component
        if colon_colon_triggered && cursor.loc.start() > prev_loc.end() {
            at_colon_colon = true;
        } else {
            return;
        }
    }

    if !at_colon_colon {
        // we are not at the last `::` but we may be at an intermediate one
        if colon_colon_triggered
            && path_index < path_entries.len()
            && cursor.loc.start() > prev_loc.end()
            && cursor.loc.end() <= path_entries[path_index].loc.start()
        {
            at_colon_colon = true;
        }
    }

    // we are at `::`, or at some component's identifier
    if at_colon_colon || path_entries[path_index].loc.contains(&cursor.loc) {
        name_chain_entry_completions(
            symbols,
            cursor,
            info,
            prev_kind,
            chain_kind,
            inside_use,
            completions,
        );
    } else {
        let component_name = path_entries[path_index];
        if let Some(next_kind) =
            next_name_chain_component_kind(symbols, info, prev_kind, component_name)
        {
            completions_for_name_chain_entry(
                symbols,
                cursor,
                info,
                ChainComponentInfo::new(component_name.loc, next_kind),
                chain_kind,
                path_entries,
                path_index + 1,
                colon_colon_triggered,
                inside_use,
                completions,
            );
        }
    }
}

/// Check if a given address represents a package within the current program.
fn is_package_address(
    symbols: &Symbols,
    info: &AliasAutocompleteInfo,
    pkg_addr: NumericalAddress,
) -> bool {
    if info.addresses.iter().any(|(_, a)| a == &pkg_addr) {
        return true;
    }

    symbols.file_mods.values().flatten().any(|mdef| {
        matches!(mdef.ident.address,
            Address::Numerical { value, .. } if value.value == pkg_addr)
    })
}

/// Check if a given name represents a package within the current program.
fn is_package_name(symbols: &Symbols, info: &AliasAutocompleteInfo, pkg_name: Name) -> bool {
    if info.addresses.contains_key(&pkg_name.value) {
        return true;
    }

    symbols
        .file_mods
        .values()
        .flatten()
        .map(|mdef| &mdef.ident)
        .any(|mod_ident| match &mod_ident.address {
            Address::NamedUnassigned(name) if name == &pkg_name => true,
            Address::Numerical {
                name: Some(name), ..
            } if name == &pkg_name => true,
            _ => false,
        })
}

/// Get all packages that could be a target of auto-completion, whether they are part of
/// `AliasAutocompleteInfo` or not.
fn all_packages(symbols: &Symbols, info: &AliasAutocompleteInfo) -> BTreeSet<String> {
    let mut addresses = BTreeSet::new();
    for (n, a) in &info.addresses {
        addresses.insert(n.to_string());
        addresses.insert(a.to_string());
    }

    symbols
        .file_mods
        .values()
        .flatten()
        .map(|mdef| &mdef.ident)
        .for_each(|mod_ident| match &mod_ident.address {
            Address::Numerical { name, value, .. } => {
                if let Some(n) = name {
                    addresses.insert(n.to_string());
                }
                addresses.insert(value.to_string());
            }
            Address::NamedUnassigned(n) => {
                addresses.insert(n.to_string());
            }
        });

    addresses
}

/// Computes the kind of the fist chain component.
fn first_name_chain_component_kind(
    symbols: &Symbols,
    info: &AliasAutocompleteInfo,
    leading_name: P::LeadingNameAccess,
) -> Option<ChainComponentKind> {
    match leading_name.value {
        P::LeadingNameAccess_::Name(n) => {
            if is_package_name(symbols, info, n) {
                Some(ChainComponentKind::Package(leading_name))
            } else if let Some(mod_ident) = info.modules.get(&n.value) {
                Some(ChainComponentKind::Module(*mod_ident))
            } else if let Some((mod_ident, member_name)) =
                info.members
                    .iter()
                    .find_map(|(alias_name, mod_ident, member_name)| {
                        if alias_name == &n.value {
                            Some((*mod_ident, member_name))
                        } else {
                            None
                        }
                    })
            {
                Some(ChainComponentKind::Member(mod_ident, member_name.value))
            } else {
                None
            }
        }
        P::LeadingNameAccess_::AnonymousAddress(addr) => {
            if is_package_address(symbols, info, addr) {
                Some(ChainComponentKind::Package(leading_name))
            } else {
                None
            }
        }
        P::LeadingNameAccess_::GlobalAddress(n) => {
            // if leading name is global address then the first component can only be a
            // package
            if is_package_name(symbols, info, n) {
                Some(ChainComponentKind::Package(leading_name))
            } else {
                None
            }
        }
    }
}

/// Computes auto-completions for module uses.
fn module_use_completions(
    symbols: &Symbols,
    cursor: &CursorContext,
    info: &AliasAutocompleteInfo,
    mod_use: &P::ModuleUse,
    package: &P::LeadingNameAccess,
    mod_name: &P::ModuleName,
) -> Vec<CompletionItem> {
    use P::ModuleUse as MU;
    let mut completions = vec![];

    let Some(mod_ident) = pkg_mod_identifiers(symbols, info, package)
        .into_iter()
        .find(|mod_ident| &mod_ident.value.module == mod_name)
    else {
        return completions;
    };

    match mod_use {
        MU::Module(_) => (), // nothing to do with just module alias
        MU::Members(members) => {
            if let Some((first_name, _)) = members.first() {
                if cursor.loc.start() > mod_name.loc().end()
                    && cursor.loc.end() <= first_name.loc.start()
                {
                    // cursor is after `::` succeeding module but before the first module member
                    completions.extend(module_member_completions(
                        symbols,
                        cursor,
                        &mod_ident,
                        ChainCompletionKind::All,
                        /* inside_use */ true,
                    ));
                    // no point in falling through to the members loop below
                    return completions;
                }
            }

            for (sp!(mloc, _), _) in members {
                if mloc.contains(&cursor.loc) {
                    // cursor is at identifier representing module member
                    completions.extend(module_member_completions(
                        symbols,
                        cursor,
                        &mod_ident,
                        ChainCompletionKind::All,
                        /* inside_use */ true,
                    ));
                    // no point checking other locations
                    break;
                }
            }
        }
        MU::Partial {
            colon_colon,
            opening_brace: _,
        } => {
            if let Some(colon_colon_loc) = colon_colon {
                if cursor.loc.start() >= colon_colon_loc.start() {
                    // cursor is on or past `::`
                    completions.extend(module_member_completions(
                        symbols,
                        cursor,
                        &mod_ident,
                        ChainCompletionKind::All,
                        /* inside_use */ true,
                    ));
                }
            }
        }
    }

    completions
}