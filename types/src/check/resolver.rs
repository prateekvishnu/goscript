#![allow(dead_code)]
use super::super::constant;
use super::super::display::ExprDisplay;
use super::super::importer::{ImportKey, Importer};
use super::super::obj::{EntityType, LangObj};
use super::super::objects::{DeclInfoKey, ObjKey, PackageKey, ScopeKey, TCObjects, TypeKey};
use super::super::package::DeclInfo;
use super::check::{Checker, FilesContext};
use super::decl;
use goscript_parser::ast;
use goscript_parser::ast::{Expr, Node};
use goscript_parser::objects::IdentKey;
use goscript_parser::position::Pos;
use goscript_parser::token::Token;
use goscript_parser::Parser;
use std::collections::{HashMap, HashSet};

impl<'a> Checker<'a> {
    pub fn collect_objects(&mut self, fctx: &mut FilesContext) {
        let mut all_imported: HashSet<PackageKey> = self
            .package(self.pkg)
            .imports()
            .iter()
            .map(|x| *x)
            .collect();
        // list of methods with non-blank names
        let mut methods: Vec<ObjKey> = Vec::new();
        for (file_num, file) in fctx.files.iter().enumerate() {
            // the original go version record a none here, what for?
            //self.result_mut().result.(file.name,  None)

            // Use the actual source file extent rather than ast::File extent since the
            // latter doesn't include comments which appear at the start or end of the file.
            // Be conservative and use the ast::File extent if we don't have a position::File.
            let mut pos = file.pos(self.ast_objs);
            let mut end = file.end(self.ast_objs);
            if let Some(f) = self.fset.file(pos) {
                pos = f.base();
                end = pos + f.size();
            }
            let parent_scope = Some(*self.package(self.pkg).scope());
            let scope_comment = fctx.file_name(file_num, self);
            let file_scope = self
                .tc_objs
                .new_scope(parent_scope, pos, end, scope_comment);
            self.result.record_scope(file, file_scope);

            for decl in file.decls.iter() {
                match decl {
                    ast::Decl::Bad(_) => {}
                    ast::Decl::Gen(gdecl) => {
                        let mut last_full_const_spec: Option<ast::Spec> = None;
                        let specs = &(*gdecl).specs;
                        for (iota, spec_key) in specs.iter().enumerate() {
                            let spec = &self.ast_objs.specs[*spec_key].clone();
                            let spec_pos = spec.pos(self.ast_objs);
                            match spec {
                                ast::Spec::Import(is) => {
                                    let ispec = &**is;
                                    let path = if let Ok(p) = self.valid_import_path(&ispec.path) {
                                        p
                                    } else {
                                        continue;
                                    };
                                    let dir = self.file_dir(file);
                                    let imp =
                                        self.import_package(ispec.path.pos, path.to_owned(), dir);

                                    // add package to list of explicit imports
                                    // (this functionality is provided as a convenience
                                    // for clients; it is not needed for type-checking)
                                    if !all_imported.contains(&imp) {
                                        all_imported.insert(imp);
                                        self.package_mut(self.pkg).add_import(imp);
                                    }

                                    // see if local name overrides imported package name
                                    let name = if ispec.name.is_some() {
                                        self.package(imp).name().clone().unwrap()
                                    } else {
                                        let ident = &self.ident(ispec.name.unwrap());
                                        if ident.name == "init" {
                                            self.error(
                                                ident.pos,
                                                "cannot declare init - must be func".to_owned(),
                                            );
                                        }
                                        ident.name.clone()
                                    };

                                    let pkg_name_obj = self.tc_objs.new_pkg_name(
                                        spec_pos,
                                        Some(self.pkg),
                                        name.to_owned(),
                                        imp,
                                    );
                                    if ispec.name.is_some() {
                                        // in a dot-import, the dot represents the package
                                        self.result.record_def(ispec.name.unwrap(), pkg_name_obj);
                                    } else {
                                        self.result.record_implicit(spec, pkg_name_obj);
                                    }

                                    // add import to file scope
                                    if name == "." {
                                        // merge imported scope with file scope
                                        let pkg_val = self.package(imp);
                                        let scope_val = self.scope(*pkg_val.scope());
                                        let elems: Vec<ObjKey> = scope_val
                                            .elems()
                                            .iter()
                                            .filter_map(|(_, v)| {
                                                if self.lobj(*v).exported() {
                                                    Some(*v)
                                                } else {
                                                    None
                                                }
                                            })
                                            .collect();
                                        for elem in elems.into_iter() {
                                            self.declare(file_scope, None, elem, 0);
                                        }
                                        // add position to set of dot-import positions for this file
                                        // (this is only needed for "imported but not used" errors)
                                        fctx.add_unused_dot_import(&file_scope, &imp, spec_pos);
                                    } else {
                                        // declare imported package object in file scope
                                        self.declare(file_scope, None, pkg_name_obj, 0);
                                    }
                                }
                                ast::Spec::Value(vs) => {
                                    let vspec = &**vs;
                                    match gdecl.token {
                                        Token::CONST => {
                                            let mut current_vspec = None;
                                            if vspec.typ.is_some() || vspec.values.len() > 0 {
                                                last_full_const_spec = Some(spec.clone());
                                                current_vspec = Some(vspec);
                                            } else {
                                                // no ValueSpec with type or init exprs,
                                                // try get the last one
                                                if let Some(spec) = &last_full_const_spec {
                                                    match spec {
                                                        ast::Spec::Value(v) => {
                                                            current_vspec = Some(&*v)
                                                        }
                                                        _ => unreachable!(),
                                                    }
                                                }
                                            }
                                            // declare all constants
                                            for (i, name) in
                                                vspec.names.clone().into_iter().enumerate()
                                            {
                                                let ident = &self.ast_objs.idents[name];
                                                let lobj = self.tc_objs.new_const(
                                                    ident.pos,
                                                    Some(self.pkg),
                                                    ident.name.clone(),
                                                    None,
                                                    constant::Value::with_i64(iota as i64),
                                                );
                                                let init = if current_vspec.is_some()
                                                    && i < current_vspec.unwrap().values.len()
                                                {
                                                    Some(current_vspec.unwrap().values[i].clone())
                                                } else {
                                                    None
                                                };
                                                let typ =
                                                    current_vspec.map(|x| x.typ.clone()).flatten();
                                                let d = self.tc_objs.new_decl_info(
                                                    file_scope, None, typ, init, None, false,
                                                );
                                                let _ = self.declare_pkg_obj(name, lobj, d);
                                            }
                                            let _ = self.arity_match(vspec, true, current_vspec);
                                        }
                                        Token::VAR => {
                                            let lhs: Vec<ObjKey> = vspec
                                                .names
                                                .iter()
                                                .map(|x| {
                                                    let ident = &self.ast_objs.idents[*x];
                                                    self.tc_objs.new_var(
                                                        ident.pos,
                                                        Some(self.pkg),
                                                        ident.name.clone(),
                                                        None,
                                                    )
                                                })
                                                .collect();
                                            let n_to_1 = vspec.values.len() == 1;
                                            let n_to_1_di = if n_to_1 {
                                                Some(self.tc_objs.new_decl_info(
                                                    file_scope,
                                                    Some(lhs.clone()),
                                                    vspec.typ.clone(),
                                                    Some(vspec.values[0].clone()),
                                                    None,
                                                    false,
                                                ))
                                            } else {
                                                None
                                            };
                                            for (i, name) in vspec.names.iter().rev().enumerate() {
                                                let di = if n_to_1 {
                                                    n_to_1_di.unwrap()
                                                } else {
                                                    self.tc_objs.new_decl_info(
                                                        file_scope,
                                                        None,
                                                        vspec.typ.clone(),
                                                        vspec.values.get(i).map(|x| x.clone()),
                                                        None,
                                                        false,
                                                    )
                                                };
                                                let _ = self.declare_pkg_obj(*name, lhs[i], di);
                                            }
                                        }
                                        _ => self.error(
                                            spec_pos,
                                            format!("invalid token {}", gdecl.token),
                                        ),
                                    }
                                }
                                ast::Spec::Type(ts) => {
                                    let tspec = &**ts;
                                    let ident = &self.ast_objs.idents[tspec.name];
                                    let lobj = self.tc_objs.new_type_name(
                                        ident.pos,
                                        Some(self.pkg),
                                        ident.name.clone(),
                                        None,
                                    );
                                    let di = self.tc_objs.new_decl_info(
                                        file_scope,
                                        None,
                                        Some(tspec.typ.clone()),
                                        None,
                                        None,
                                        tspec.assign > 0,
                                    );
                                    let _ = self.declare_pkg_obj(tspec.name, lobj, di);
                                }
                            }
                        }
                    }
                    ast::Decl::Func(fdkey) => {
                        let fdecl = &self.ast_objs.fdecls[*fdkey];
                        let ident_key = fdecl.name;
                        let ident = &self.ast_objs.idents[ident_key];
                        let lobj = self.tc_objs.new_func(
                            ident.pos,
                            Some(self.pkg),
                            ident.name.clone(),
                            None,
                        );
                        if fdecl.recv.is_none() {
                            // regular function
                            let scope = *self.package(self.pkg).scope();
                            if ident.name == "init" {
                                self.tc_objs.lobjs[lobj].set_parent(Some(scope));
                                self.result.record_def(ident_key, lobj);
                                if fdecl.body.is_none() {
                                    self.error(ident.pos, "missing function body".to_owned());
                                }
                            } else {
                                self.declare(scope, Some(ident_key), lobj, 0);
                            }
                        } else {
                            // method
                            // (Methods with blank _ names are never found; no need to collect
                            // them for later type association. They will still be type-checked
                            // with all the other functions.)
                            if ident.name != "_" {
                                methods.push(lobj);
                            }
                            self.result.record_def(ident_key, lobj);
                        }
                        let di = self.tc_objs.new_decl_info(
                            file_scope,
                            None,
                            None,
                            None,
                            Some(*fdkey),
                            false,
                        );
                        self.obj_map.insert(lobj, di);
                        let order = self.obj_map.len() as u32;
                        self.lobj_mut(lobj).set_order(order);
                    }
                }
            }
        }
        // verify that objects in package and file scopes have different names
        let pkg_scope = self.scope(*self.package(self.pkg).scope());
        for s in pkg_scope.children().iter() {
            for (_, okey) in self.scope(*s).elems() {
                let obj_val = self.lobj(*okey);
                if let Some(alt) = pkg_scope.lookup(obj_val.name()) {
                    let alt_val = self.lobj(*alt);
                    match obj_val.entity_type() {
                        EntityType::PkgName(pkey, _) => {
                            let pkg_val = self.package(*pkey);
                            self.error(
                                *alt_val.pos(),
                                format!(
                                    "{} already declared through import of {}",
                                    alt_val.name(),
                                    pkg_val
                                ),
                            );
                        }
                        _ => {
                            let pkg_val = self.package(obj_val.pkg().unwrap());
                            self.error(
                                *alt_val.pos(),
                                format!(
                                    "{} already declared through dot-import of {}",
                                    alt_val.name(),
                                    pkg_val
                                ),
                            );
                        }
                    }
                    self.report_alt_decl(okey);
                }
            }
        }
        // Now that we have all package scope objects and all methods,
        // associate methods with receiver base type name where possible.
        // Ignore methods that have an invalid receiver. They will be
        // type-checked later, with regular functions.
        for f in methods.into_iter() {
            let fdkey = &self.tc_objs.decls[self.obj_map[&f]].fdecl.unwrap();
            let fdecl = &self.ast_objs.fdecls[*fdkey];
            if let Some(fl) = &fdecl.recv {
                // f is a method
                // receiver may be of the form T or *T, possibly with parentheses
                let mut typ = Parser::unparen(&self.ast_objs.fields[fl.list[0]].typ);
                if let Expr::Star(t) = typ {
                    typ = Parser::unparen(&t.expr);
                }
                if let Expr::Ident(ident) = typ {
                    // base is a potential base type name; determine
                    // "underlying" defined type and associate f with it
                    if let Some(tname) = self.resolve_base_type_name(ident) {
                        fctx.methods.entry(tname).or_default().push(f);
                    }
                }
            }
        }
    }

    /// arity_match checks that the lhs and rhs of a const or var decl
    /// have the appropriate number of names and init exprs.
    /// set 'cst' as true for const decls, 'init' is not used for var decls.
    pub fn arity_match(
        &self,
        s: &ast::ValueSpec,
        cst: bool,
        init: Option<&ast::ValueSpec>,
    ) -> Result<(), ()> {
        let l = s.names.len();
        let r = if cst {
            if let Some(i) = init {
                i.values.len()
            } else {
                0
            }
        } else {
            s.values.len()
        };
        if !cst && r == 0 {
            // var decl w/o init expr
            if s.typ.is_none() {
                self.error(
                    self.ident(s.names[0]).pos,
                    "missing type or init expr".to_string(),
                );
                return Err(());
            }
        } else if l < r {
            if init.is_none() {
                let expr = &s.values[l];
                self.error(
                    expr.pos(self.ast_objs),
                    format!("extra init expr {}", ExprDisplay::new(expr, self.ast_objs)),
                );
                return Err(());
            } else {
                let pos = self.ident(init.unwrap().names[0]).pos;
                self.error(
                    self.ident(s.names[0]).pos,
                    format!("extra init expr at {}", self.position(pos)),
                );
                return Err(());
            }
        } else if l > r && (cst || r != 1) {
            let ident = self.ident(s.names[r]);
            self.error(ident.pos, format!("missing init expr for {}", ident.name));
            return Err(());
        }
        Ok(())
    }

    fn resolve_base_type_name(&self, ikey: &IdentKey) -> Option<ObjKey> {
        unimplemented!()
    }

    fn valid_import_path(&self, blit: &'a ast::BasicLit) -> Result<&'a str, ()> {
        let path = blit.token.get_literal();
        let pos = blit.pos;
        if path.len() < 3 || (!path.starts_with('"') || !path.ends_with('"')) {
            self.error(pos, format!("invalid import path: {}", path));
            return Err(());
        }
        let result = &path[1..path.len() - 1];
        let mut illegal_chars: Vec<char> = r##"!"#$%&'()*,:;<=>?[\]^{|}`"##.chars().collect();
        illegal_chars.push('\u{FFFD}');
        if let Some(c) = illegal_chars
            .iter()
            .find(|&x| x.is_ascii_graphic() || x.is_whitespace() || result.contains(*x))
        {
            self.error(pos, format!("invalid character: {}", c));
            return Err(());
        }
        Ok(result)
    }

    /// declare_pkg_obj declares obj in the package scope, records its ident -> obj mapping,
    /// and updates check.objMap. The object must not be a function or method.
    fn declare_pkg_obj(
        &mut self,
        ikey: IdentKey,
        okey: ObjKey,
        dkey: DeclInfoKey,
    ) -> Result<(), ()> {
        let ident = self.ident(ikey);
        let lobj = self.lobj(okey);
        assert_eq!(&ident.name, lobj.name());
        // spec: "A package-scope or file-scope identifier with name init
        // may only be declared to be a function with this (func()) signature."
        if &ident.name == "init" {
            self.error(ident.pos, "cannot declare init - must be func".to_owned());
            return Err(());
        }
        // spec: "The main package must have package name main and declare
        // a function main that takes no arguments and returns no value."
        let pkg_name = self.package(self.pkg).name();
        if &ident.name == "main" && pkg_name.is_some() && pkg_name.as_ref().unwrap() == "main" {
            self.error(ident.pos, "cannot declare main - must be func".to_owned());
            return Err(());
        }
        let scope = *self.package(self.pkg).scope();
        self.declare(scope, Some(ikey), okey, 0);
        self.obj_map.insert(okey, dkey);
        let order = self.obj_map.len() as u32;
        self.lobj_mut(okey).set_order(order);
        Ok(())
    }

    fn import_package(&mut self, pos: Pos, path: String, dir: String) -> PackageKey {
        // If we already have a package for the given (path, dir)
        // pair, use it instead of doing a full import.
        // Checker.imp_map only caches packages that are marked Complete
        // or fake (dummy packages for failed imports). Incomplete but
        // non-fake packages do require an import to complete them.
        let key = ImportKey::new(path.clone(), dir);
        if let Some(imp) = self.imp_map.get(&key) {
            return *imp;
        }

        let mut imported = self.new_importer(pos).import(&key);
        if imported.is_err() {
            self.error(pos, format!("could not import {}", &path));
            // create a new fake package
            let mut name = &path[0..path.len()];
            if name.len() > 0 && name.ends_with('/') {
                name = &name[0..name.len() - 1];
            }
            if let Some(i) = name.rfind('/') {
                name = &name[i..name.len()]
            }
            let pkg = self.tc_objs.new_package(path.clone());
            self.package_mut(pkg).mark_fake_with_name(name.to_owned());
            imported = Ok(pkg);
        }
        self.imp_map.insert(key, imported.unwrap());
        imported.unwrap()
    }

    fn file_dir(&self, file: &ast::File) -> String {
        let path = self.fset.file(self.ident(file.name).pos).unwrap().name();
        if let Some((i, _)) = path.rmatch_indices(&['/', '\\'][..]).next() {
            if i > 0 {
                return path[0..i].to_owned();
            }
        }
        ".".to_owned()
    }
}
