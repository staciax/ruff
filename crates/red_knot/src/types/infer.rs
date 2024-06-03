#![allow(dead_code)]

use ruff_python_ast as ast;
use ruff_python_ast::AstNode;

use crate::db::{QueryResult, SemanticDb, SemanticJar};

use crate::module::{resolve_module, ModuleName};
use crate::parse::parse;
use crate::symbols::{
    resolve_global_symbol, symbol_table, Definition, GlobalSymbolId, ImportDefinition,
    ImportFromDefinition,
};
use crate::types::{ModuleTypeId, Type};
use crate::{FileId, Name};

// FIXME: Figure out proper dead-lock free synchronisation now that this takes `&db` instead of `&mut db`.
/// Resolve the public-facing type for a symbol (the type seen by other scopes: other modules, or
/// nested functions). Because calls to nested functions and imports can occur anywhere in control
/// flow, this type must be conservative and consider all definitions of the symbol that could
/// possibly be seen by another scope. Currently we take the most conservative approach, which is
/// the union of all definitions. We may be able to narrow this in future to eliminate definitions
/// which can't possibly (or at least likely) be seen by any other scope, so that e.g. we could
/// infer `Literal["1"]` instead of `Literal[1] | Literal["1"]` for `x` in `x = x; x = str(x);`.
#[tracing::instrument(level = "trace", skip(db))]
pub fn infer_symbol_public_type(db: &dyn SemanticDb, symbol: GlobalSymbolId) -> QueryResult<Type> {
    let symbols = symbol_table(db, symbol.file_id)?;
    let defs = symbols.definitions(symbol.symbol_id);
    let jar: &SemanticJar = db.jar()?;

    if let Some(ty) = jar.type_store.get_cached_symbol_public_type(symbol) {
        return Ok(ty);
    }

    let mut tys = defs
        .iter()
        .map(|def| infer_definition_type(db, symbol, def.clone()))
        .peekable();
    let ty = if let Some(first) = tys.next() {
        if tys.peek().is_some() {
            Type::Union(jar.type_store.add_union(
                symbol.file_id,
                &Iterator::chain([first].into_iter(), tys).collect::<QueryResult<Vec<_>>>()?,
            ))
        } else {
            first?
        }
    } else {
        Type::Unknown
    };

    jar.type_store.cache_symbol_public_type(symbol, ty);

    // TODO record dependencies
    Ok(ty)
}

#[tracing::instrument(level = "trace", skip(db))]
pub fn infer_definition_type(
    db: &dyn SemanticDb,
    symbol: GlobalSymbolId,
    definition: Definition,
) -> QueryResult<Type> {
    let jar: &SemanticJar = db.jar()?;
    let type_store = &jar.type_store;
    let file_id = symbol.file_id;

    match definition {
        Definition::Import(ImportDefinition {
            module: module_name,
        }) => {
            if let Some(module) = resolve_module(db, module_name.clone())? {
                Ok(Type::Module(ModuleTypeId { module, file_id }))
            } else {
                Ok(Type::Unknown)
            }
        }
        Definition::ImportFrom(ImportFromDefinition {
            module,
            name,
            level,
        }) => {
            // TODO relative imports
            assert!(matches!(level, 0));
            let module_name = ModuleName::new(module.as_ref().expect("TODO relative imports"));
            if let Some(remote_symbol) = resolve_global_symbol(db, module_name, &name)? {
                infer_symbol_public_type(db, remote_symbol)
            } else {
                Ok(Type::Unknown)
            }
        }
        Definition::ClassDef(node_key) => {
            if let Some(ty) = type_store.get_cached_node_type(file_id, node_key.erased()) {
                Ok(ty)
            } else {
                let parsed = parse(db.upcast(), file_id)?;
                let ast = parsed.ast();
                let table = symbol_table(db, file_id)?;
                let node = node_key.resolve_unwrap(ast.as_any_node_ref());

                let mut bases = Vec::with_capacity(node.bases().len());

                for base in node.bases() {
                    bases.push(infer_expr_type(db, file_id, base)?);
                }
                let scope_id = table.scope_id_for_node(node_key.erased());
                let ty = Type::Class(type_store.add_class(file_id, &node.name.id, scope_id, bases));
                type_store.cache_node_type(file_id, *node_key.erased(), ty);
                Ok(ty)
            }
        }
        Definition::FunctionDef(node_key) => {
            if let Some(ty) = type_store.get_cached_node_type(file_id, node_key.erased()) {
                Ok(ty)
            } else {
                let parsed = parse(db.upcast(), file_id)?;
                let ast = parsed.ast();
                let table = symbol_table(db, file_id)?;
                let node = node_key
                    .resolve(ast.as_any_node_ref())
                    .expect("node key should resolve");

                let decorator_tys = node
                    .decorator_list
                    .iter()
                    .map(|decorator| infer_expr_type(db, file_id, &decorator.expression))
                    .collect::<QueryResult<_>>()?;
                let scope_id = table.scope_id_for_node(node_key.erased());
                let ty = type_store
                    .add_function(
                        file_id,
                        &node.name.id,
                        symbol.symbol_id,
                        scope_id,
                        decorator_tys,
                    )
                    .into();
                type_store.cache_node_type(file_id, *node_key.erased(), ty);
                Ok(ty)
            }
        }
        Definition::Assignment(node_key) => {
            let parsed = parse(db.upcast(), file_id)?;
            let ast = parsed.ast();
            let node = node_key.resolve_unwrap(ast.as_any_node_ref());
            // TODO handle unpacking assignment correctly (here and for AnnotatedAssignment case, below)
            infer_expr_type(db, file_id, &node.value)
        }
        Definition::AnnotatedAssignment(node_key) => {
            let parsed = parse(db.upcast(), file_id)?;
            let ast = parsed.ast();
            let node = node_key.resolve_unwrap(ast.as_any_node_ref());
            // TODO actually look at the annotation
            let Some(value) = &node.value else {
                return Ok(Type::Unknown);
            };
            // TODO handle unpacking assignment correctly (here and for Assignment case, above)
            infer_expr_type(db, file_id, value)
        }
    }
}

fn infer_expr_type(db: &dyn SemanticDb, file_id: FileId, expr: &ast::Expr) -> QueryResult<Type> {
    // TODO cache the resolution of the type on the node
    let symbols = symbol_table(db, file_id)?;
    match expr {
        ast::Expr::NumberLiteral(ast::ExprNumberLiteral { value, .. }) => {
            match value {
                ast::Number::Int(n) => {
                    // TODO support big int literals
                    Ok(n.as_i64().map(Type::IntLiteral).unwrap_or(Type::Unknown))
                }
                // TODO builtins.float or builtins.complex
                _ => Ok(Type::Unknown),
            }
        }
        ast::Expr::Name(name) => {
            // TODO look up in the correct scope, don't assume global
            if let Some(symbol_id) = symbols.root_symbol_id_by_name(&name.id) {
                // TODO should use only reachable definitions, not public type
                infer_symbol_public_type(db, GlobalSymbolId { file_id, symbol_id })
            } else {
                Ok(Type::Unknown)
            }
        }
        ast::Expr::Attribute(ast::ExprAttribute { value, attr, .. }) => {
            let value_type = infer_expr_type(db, file_id, value)?;
            let attr_name = &Name::new(&attr.id);
            value_type
                .get_member(db, attr_name)
                .map(|ty| ty.unwrap_or(Type::Unknown))
        }
        _ => todo!("full expression type resolution"),
    }
}

#[cfg(test)]
mod tests {
    use crate::db::tests::TestDb;
    use crate::db::{HasJar, SemanticJar};
    use crate::module::{
        resolve_module, set_module_search_paths, ModuleName, ModuleSearchPath, ModuleSearchPathKind,
    };
    use crate::symbols::{symbol_table, GlobalSymbolId};
    use crate::types::{infer_symbol_public_type, Type};
    use crate::Name;

    // TODO with virtual filesystem we shouldn't have to write files to disk for these
    // tests

    struct TestCase {
        temp_dir: tempfile::TempDir,
        db: TestDb,

        src: ModuleSearchPath,
    }

    fn create_test() -> std::io::Result<TestCase> {
        let temp_dir = tempfile::tempdir()?;

        let src = temp_dir.path().join("src");
        std::fs::create_dir(&src)?;
        let src = ModuleSearchPath::new(src.canonicalize()?, ModuleSearchPathKind::FirstParty);

        let roots = vec![src.clone()];

        let mut db = TestDb::default();
        set_module_search_paths(&mut db, roots);

        Ok(TestCase { temp_dir, db, src })
    }

    #[test]
    fn follow_import_to_class() -> anyhow::Result<()> {
        let case = create_test()?;
        let db = &case.db;

        let a_path = case.src.path().join("a.py");
        let b_path = case.src.path().join("b.py");
        std::fs::write(a_path, "from b import C as D; E = D")?;
        std::fs::write(b_path, "class C: pass")?;
        let a_file = resolve_module(db, ModuleName::new("a"))?
            .expect("module should be found")
            .path(db)?
            .file();
        let a_syms = symbol_table(db, a_file)?;
        let e_sym = a_syms
            .root_symbol_id_by_name("E")
            .expect("E symbol should be found");

        let ty = infer_symbol_public_type(
            db,
            GlobalSymbolId {
                file_id: a_file,
                symbol_id: e_sym,
            },
        )?;

        let jar = HasJar::<SemanticJar>::jar(db)?;
        assert!(matches!(ty, Type::Class(_)));
        assert_eq!(format!("{}", ty.display(&jar.type_store)), "Literal[C]");

        Ok(())
    }

    #[test]
    fn resolve_base_class_by_name() -> anyhow::Result<()> {
        let case = create_test()?;
        let db = &case.db;

        let path = case.src.path().join("mod.py");
        std::fs::write(path, "class Base: pass\nclass Sub(Base): pass")?;
        let file = resolve_module(db, ModuleName::new("mod"))?
            .expect("module should be found")
            .path(db)?
            .file();
        let syms = symbol_table(db, file)?;
        let sym = syms
            .root_symbol_id_by_name("Sub")
            .expect("Sub symbol should be found");

        let ty = infer_symbol_public_type(
            db,
            GlobalSymbolId {
                file_id: file,
                symbol_id: sym,
            },
        )?;

        let Type::Class(class_id) = ty else {
            panic!("Sub is not a Class")
        };
        let jar = HasJar::<SemanticJar>::jar(db)?;
        let base_names: Vec<_> = jar
            .type_store
            .get_class(class_id)
            .bases()
            .iter()
            .map(|base_ty| format!("{}", base_ty.display(&jar.type_store)))
            .collect();

        assert_eq!(base_names, vec!["Literal[Base]"]);

        Ok(())
    }

    #[test]
    fn resolve_method() -> anyhow::Result<()> {
        let case = create_test()?;
        let db = &case.db;

        let path = case.src.path().join("mod.py");
        std::fs::write(path, "class C:\n  def f(self): pass")?;
        let file = resolve_module(db, ModuleName::new("mod"))?
            .expect("module should be found")
            .path(db)?
            .file();
        let syms = symbol_table(db, file)?;
        let sym = syms
            .root_symbol_id_by_name("C")
            .expect("C symbol should be found");

        let ty = infer_symbol_public_type(
            db,
            GlobalSymbolId {
                file_id: file,
                symbol_id: sym,
            },
        )?;

        let Type::Class(class_id) = ty else {
            panic!("C is not a Class");
        };

        let member_ty = class_id
            .get_own_class_member(db, &Name::new("f"))
            .expect("C.f to resolve");

        let Some(Type::Function(func_id)) = member_ty else {
            panic!("C.f is not a Function");
        };

        let jar = HasJar::<SemanticJar>::jar(db)?;
        let function = jar.type_store.get_function(func_id);
        assert_eq!(function.name(), "f");

        Ok(())
    }

    #[test]
    fn resolve_module_member() -> anyhow::Result<()> {
        let case = create_test()?;
        let db = &case.db;

        let a_path = case.src.path().join("a.py");
        let b_path = case.src.path().join("b.py");
        std::fs::write(a_path, "import b; D = b.C")?;
        std::fs::write(b_path, "class C: pass")?;
        let a_file = resolve_module(db, ModuleName::new("a"))?
            .expect("module should be found")
            .path(db)?
            .file();
        let a_syms = symbol_table(db, a_file)?;
        let d_sym = a_syms
            .root_symbol_id_by_name("D")
            .expect("D symbol should be found");

        let ty = infer_symbol_public_type(
            db,
            GlobalSymbolId {
                file_id: a_file,
                symbol_id: d_sym,
            },
        )?;

        let jar = HasJar::<SemanticJar>::jar(db)?;
        assert!(matches!(ty, Type::Class(_)));
        assert_eq!(format!("{}", ty.display(&jar.type_store)), "Literal[C]");
        Ok(())
    }

    #[test]
    fn resolve_literal() -> anyhow::Result<()> {
        let case = create_test()?;
        let db = &case.db;

        let path = case.src.path().join("a.py");
        std::fs::write(path, "x = 1")?;
        let file = resolve_module(db, ModuleName::new("a"))?
            .expect("module should be found")
            .path(db)?
            .file();
        let syms = symbol_table(db, file)?;
        let x_sym = syms
            .root_symbol_id_by_name("x")
            .expect("x symbol should be found");

        let ty = infer_symbol_public_type(
            db,
            GlobalSymbolId {
                file_id: file,
                symbol_id: x_sym,
            },
        )?;

        let jar = HasJar::<SemanticJar>::jar(db)?;
        assert!(matches!(ty, Type::IntLiteral(_)));
        assert_eq!(format!("{}", ty.display(&jar.type_store)), "Literal[1]");
        Ok(())
    }

    #[test]
    fn resolve_union() -> anyhow::Result<()> {
        let case = create_test()?;
        let db = &case.db;

        let path = case.src.path().join("a.py");
        std::fs::write(path, "if flag:\n  x = 1\nelse:\n  x = 2")?;
        let file = resolve_module(db, ModuleName::new("a"))?
            .expect("module should be found")
            .path(db)?
            .file();
        let syms = symbol_table(db, file)?;
        let x_sym = syms
            .root_symbol_id_by_name("x")
            .expect("x symbol should be found");

        let ty = infer_symbol_public_type(
            db,
            GlobalSymbolId {
                file_id: file,
                symbol_id: x_sym,
            },
        )?;

        let jar = HasJar::<SemanticJar>::jar(db)?;
        assert!(matches!(ty, Type::Union(_)));
        assert_eq!(
            format!("{}", ty.display(&jar.type_store)),
            "(Literal[1] | Literal[2])"
        );
        Ok(())
    }
}
