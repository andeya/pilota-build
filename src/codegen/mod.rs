use std::{
    io::Write,
    ops::Deref,
    path::{Path, PathBuf},
    sync::Arc,
};
use std::collections::HashMap;

use dashmap::DashMap;
use faststr::FastStr;
use itertools::Itertools;
use quote::quote;
use rayon::prelude::IntoParallelRefIterator;

use pkg_tree::PkgNode;
use traits::CodegenBackend;

use crate::{
    Context,
    db::RirDatabase,
    fmt::fmt_file,
    middle::{
        self,
        adjust::Adjust,
        context::{Mode, tls::CUR_ITEM},
        rir::{self, Field},
        ty::TyKind,
    },
    symbol::{DefId, EnumRepr}, Symbol,
    tags::EnumMode,
};

use self::workspace::Workspace;

pub(crate) mod pkg_tree;
pub mod toml;
pub(crate) mod traits;

mod workspace;

pub mod protobuf;
pub mod thrift;

#[derive(Clone)]
pub struct Codegen<B> {
    backend: B,
}

impl<B> Deref for Codegen<B>
    where
        B: CodegenBackend,
{
    type Target = Context;

    fn deref(&self) -> &Self::Target {
        self.backend.cx()
    }
}

impl<B> Codegen<B> {
    pub fn new(backend: B) -> Self {
        Codegen { backend }
    }
}

#[derive(Clone, Copy)]
pub enum CodegenKind {
    Direct,
    RePub,
}

#[derive(Clone, Copy)]
pub struct CodegenItem {
    def_id: DefId,
    kind: CodegenKind,
}

impl From<DefId> for CodegenItem {
    fn from(value: DefId) -> Self {
        CodegenItem {
            def_id: value,
            kind: CodegenKind::Direct,
        }
    }
}

pub fn is_raw_ptr_field(f: &Arc<Field>, adjust: Option<&Adjust>) -> bool {
    f.is_optional() || adjust.map_or(false, |adjust| adjust.boxed())
}


impl<B> Codegen<B>
    where
        B: CodegenBackend + Send,
{
    fn check_scalar_ty(&self, def_id: DefId, record: &mut HashMap<DefId, bool>) -> bool {
        if let Some(x) = record.get(&def_id) {
            return x.clone();
        }
        let item = self.item(def_id).unwrap();
        match &*item {
            middle::rir::Item::Enum(e) => { false }
            middle::rir::Item::NewType(t) => { false }
            middle::rir::Item::Const(c) => { false }
            middle::rir::Item::Mod(m) => { false }
            middle::rir::Item::Message(s) => {
                for field in &s.fields {
                    if field.is_optional() || self.with_adjust(field.did, |adjust| {
                        adjust.map_or(false, |adjust| adjust.boxed())
                    }) {
                        field.ty.in_stack.write().unwrap().insert(false);
                        s.all_in_stack.write().unwrap().insert(false);
                        continue;
                    }
                    if field.ty.setted_in_stack_field() {
                        let is_in_stack = field.ty.is_in_stack();
                        if !is_in_stack {
                            s.all_in_stack.write().unwrap().insert(false);
                        }
                        continue;
                    }
                    match &field.ty.kind {
                        TyKind::Path(path) => {
                            let x = self.check_scalar_ty(path.did, record);
                            field.ty.in_stack.write().unwrap().get_or_insert(x);
                        }
                        _ => {
                            panic!("should not execute here")
                        }
                    }
                }
                let mut all_in_stack = s.all_in_stack.write().unwrap();
                if all_in_stack.is_none() {
                    all_in_stack.insert(true);
                }
                record.insert(def_id, all_in_stack.clone().unwrap());
                return all_in_stack.unwrap();
            }
            middle::rir::Item::Service(s) => {
                let mut is_scalar = true;
                for method in &s.methods {
                    for arg in &method.args {
                        if arg.ty.setted_in_stack_field() {
                            is_scalar = arg.ty.is_in_stack();
                        } else {
                            match &arg.ty.kind {
                                TyKind::Path(path) => {
                                    let x = self.check_scalar_ty(path.did, record);
                                    arg.ty.in_stack.write().unwrap().get_or_insert(x);
                                    if !x {
                                        is_scalar = false;
                                    }
                                }
                                _ => {
                                    panic!("should not execute here")
                                }
                            }
                        }
                    }
                    if method.ret.setted_in_stack_field() {
                        is_scalar = method.ret.is_in_stack();
                    } else {
                        match &method.ret.kind {
                            TyKind::Path(path) => {
                                let x = self.check_scalar_ty(path.did, record);
                                method.ret.in_stack.write().unwrap().get_or_insert(x);
                                if !x {
                                    is_scalar = false;
                                }
                            }
                            _ => {
                                panic!("should not execute here")
                            }
                        }
                    }
                }
                record.insert(def_id, is_scalar);
                is_scalar
            }
        }
    }
    pub fn write_struct(&self, def_id: DefId, stream: &mut String, s: &rir::Message) {
        let name = self.rust_name(def_id);
        let fields = s
            .fields
            .iter()
            .map(|f| {
                let name = self.rust_name(f.did);
                self.with_adjust(f.did, |adjust| {
                    let ty = self.codegen_item_ty(f.ty.kind.clone());
                    let mut ty = format!("{ty}");

                    if let Some(adjust) = adjust {
                        if adjust.boxed() {
                            ty = format!("::std::boxed::Box<{ty}>")
                        }
                    }

                    if f.is_optional() {
                        ty = format!("::std::option::Option<{ty}>")
                    }

                    let attrs = adjust.iter().flat_map(|a| a.attrs()).join("");

                    format! {
                        r#"{attrs}
                        pub {name}: {ty},"#
                    }
                })
            })
            .join("\n");

        let repr_c_attr = if s.is_all_in_stack() { "#[repr(C)]" } else { "" };

        stream.push_str(&format! {
            r#"{repr_c_attr}
            #[derive(Clone, PartialEq)]
            pub struct {name} {{
                {fields}
            }}"#
        });

        self.backend.codegen_struct_impl(def_id, stream, s);
    }

    pub fn write_item(&self, stream: &mut String, item: CodegenItem) {
        CUR_ITEM.set(&item.def_id, || match item.kind {
            CodegenKind::Direct => {
                let def_id = item.def_id;
                let item = self.item(def_id).unwrap();
                tracing::trace!("write item {}", item.symbol_name());
                self.with_adjust(def_id, |adjust| {
                    let attrs = adjust.iter().flat_map(|a| a.attrs()).join("\n");

                    let impls = adjust
                        .iter()
                        .flat_map(|a| &a.nested_items)
                        .sorted()
                        .join("\n");
                    stream.push_str(&impls);
                    stream.push_str(&attrs);
                });

                match &*item {
                    middle::rir::Item::Message(s) => self.write_struct(def_id, stream, s),
                    middle::rir::Item::Enum(e) => self.write_enum(def_id, stream, e),
                    middle::rir::Item::Service(s) => self.write_service(def_id, stream, s),
                    middle::rir::Item::NewType(t) => self.write_new_type(def_id, stream, t),
                    middle::rir::Item::Const(c) => self.write_const(def_id, stream, c),
                    middle::rir::Item::Mod(m) => {
                        let mut inner = Default::default();
                        m.items
                            .iter()
                            .for_each(|def_id| self.write_item(&mut inner, (*def_id).into()));

                        stream.push_str(&inner);
                        // let name = self.rust_name(def_id);
                        // stream.push_str(&format! {
                        //     r#"pub mod {name} {{
                        //     {inner}
                        // }}"#
                        // })
                    }
                };
            }
            CodegenKind::RePub => {
                let path = self.item_path(item.def_id).join("::");
                stream.push_str(format!("pub use ::{};", path).as_str());
            }
        })
    }

    pub fn write_enum_as_new_type(
        &self,
        def_id: DefId,
        stream: &mut String,
        e: &middle::rir::Enum,
    ) {
        let name = self.rust_name(def_id);

        let repr = match e.repr {
            Some(EnumRepr::I32) => quote!(i32),
            _ => panic!(),
        };

        let variants = e
            .variants
            .iter()
            .map(|v| {
                let name = self.rust_name(v.did);

                let discr = v.discr.unwrap();
                let discr = match e.repr {
                    Some(EnumRepr::I32) => discr as i32,
                    None => panic!(),
                };
                format!("pub const {name}: Self = Self({discr});")
            })
            .join("");

        stream.push_str(&format! {
            r#"#[derive(Clone, PartialEq, Copy)]
            #[repr(transparent)]
            pub struct {name}({repr});

            impl {name} {{
                {variants}

                pub fn inner(&self) -> {repr} {{
                    self.0
                }}
            }}

            impl ::std::convert::From<{repr}> for {name} {{
                fn from(value: {repr}) -> Self {{
                    Self(value)
                }}
            }}"#
        });

        self.backend.codegen_enum_impl(def_id, stream, e);
    }

    pub fn write_enum(&self, def_id: DefId, stream: &mut String, e: &middle::rir::Enum) {
        if self
            .node_tags(def_id)
            .unwrap()
            .get::<EnumMode>()
            .filter(|s| **s == EnumMode::NewType)
            .is_some()
        {
            return self.write_enum_as_new_type(def_id, stream, e);
        }
        let name = self.rust_name(def_id);

        let mut repr = if e.variants.is_empty() {
            quote! {}
        } else {
            match e.repr {
                Some(EnumRepr::I32) => quote! {
                   #[repr(i32)]
                },
                None => quote! {},
            }
        };

        if e.repr.is_some() {
            repr.extend(quote! { #[derive(Copy)] })
        }

        let variants = e
            .variants
            .iter()
            .map(|v| {
                let name = self.rust_name(v.did);

                self.with_adjust(v.did, |adjust| {
                    let attrs = adjust.iter().flat_map(|a| a.attrs()).join("\n");

                    let fields = v
                        .fields
                        .iter()
                        .map(|ty| self.codegen_item_ty(ty.kind.clone()).to_string())
                        .join(",");

                    let fields_stream = if fields.is_empty() {
                        Default::default()
                    } else {
                        format!("({fields})")
                    };

                    let discr = v
                        .discr
                        .map(|x| {
                            let x = isize::try_from(x).unwrap();
                            let x = match e.repr {
                                Some(EnumRepr::I32) => x as i32,
                                None => panic!(),
                            };
                            format!("={x}")
                        })
                        .unwrap_or_default();

                    format!(
                        r#"{attrs}
                        {name} {fields_stream} {discr},"#
                    )
                })
            })
            .join("\n");

        stream.push_str(&format! {
            r#"
            #[derive(Clone, PartialEq)]
            {repr}
            pub enum {name} {{
                {variants}
            }}
            "#
        });

        self.backend.codegen_enum_impl(def_id, stream, e);
    }

    pub fn write_service(&self, def_id: DefId, stream: &mut String, s: &middle::rir::Service) {
        let name = self.rust_name(def_id);
        let methods = self.service_methods(def_id);

        let methods = methods
            .iter()
            .map(|m| self.backend.codegen_service_method(def_id, m))
            .filter(|code| !code.is_empty())
            .join("\n");
        if !methods.is_empty() {
            stream.push_str(&format! {r#"
            pub trait {name} {{
                {methods}
            }}
            "#});
        }
        self.backend.codegen_service_impl(def_id, stream, s);
    }

    pub fn write_new_type(&self, def_id: DefId, stream: &mut String, t: &middle::rir::NewType) {
        let name = self.rust_name(def_id);
        let ty = self.codegen_item_ty(t.ty.kind.clone());
        stream.push_str(&format! {
            r#"
            #[derive(Clone, PartialEq)]
            pub struct {name}(pub {ty});

            impl ::std::ops::Deref for {name} {{
                type Target = {ty};

                fn deref(&self) -> &Self::Target {{
                    &self.0
                }}
            }}

            impl From<{ty}> for {name} {{
                fn from(v: {ty}) -> Self {{
                    Self(v)
                }}
            }}
            "#
        });
        self.backend.codegen_newtype_impl(def_id, stream, t);
    }

    pub fn write_const(&self, did: DefId, stream: &mut String, c: &middle::rir::Const) {
        let mut ty = self.codegen_ty(did);

        let name = self.rust_name(did);

        stream.push_str(&self.def_lit(&name, &c.lit, &mut ty).unwrap())
    }

    pub fn write_workspace(self, base_dir: PathBuf) -> anyhow::Result<()> {
        let ws = Workspace::new(base_dir, self);
        ws.write_crates()
    }

    pub fn write_items<'a>(&self, stream: &mut String, items: impl Iterator<Item=CodegenItem>)
        where
            B: Send,
    {
        use rayon::iter::ParallelIterator;

        let mods = items.into_group_map_by(|CodegenItem { def_id, .. }| {
            let path = Arc::from_iter(self.mod_path(*def_id).iter().map(|s| s.0.clone()));
            tracing::debug!("ths path of {:?} is {:?}", def_id, path);
            match &*self.mode {
                Mode::Workspace(_) => Arc::from(&path[1..]), /* the first element for
                                                                * workspace */
                // path is crate name
                Mode::SingleFile { .. } => path,
            }
        });

        let mut pkgs: DashMap<Arc<[FastStr]>, String> = Default::default();

        let this = self.clone();

        mods.par_iter().for_each_with(this, |this, (p, def_ids)| {
            let mut stream = pkgs.entry(p.clone()).or_default();

            let span = tracing::span!(tracing::Level::TRACE, "write_mod", path = ?p);

            let _enter = span.enter();
            def_ids.iter().for_each(|def_id| {
                match def_id.kind {
                    CodegenKind::Direct => {
                        this.check_scalar_ty(def_id.def_id, &mut HashMap::new());
                    }
                    _ => {}
                }
            });
            for def_id in def_ids.iter() {
                this.write_item(&mut stream, *def_id)
            }
        });

        fn write_stream(
            pkgs: &mut DashMap<Arc<[FastStr]>, String>,
            stream: &mut String,
            nodes: &[PkgNode],
        ) {
            for node in nodes {
                let mut inner_stream = String::default();
                if let Some((_, node_stream)) = pkgs.remove(&node.path) {
                    inner_stream.push_str(&node_stream);
                }

                write_stream(pkgs, &mut inner_stream, &node.children);
                let name = node.ident();
                if name.clone().unwrap_or_default() == "" {
                    stream.push_str(&inner_stream);
                    return;
                }

                stream.push_str(&inner_stream);
                // let name = Symbol::from(name.unwrap());
                // stream.push_str(&format! {
                //     r#"
                //     pub mod {name} {{
                //         {inner_stream}
                //     }}
                //     "#
                // });
            }
        }

        let keys = pkgs.iter().map(|kv| kv.key().clone()).collect_vec();
        let pkg_node = PkgNode::from_pkgs(&keys.iter().map(|s| &**s).collect_vec());
        tracing::debug!(?pkg_node);

        write_stream(&mut pkgs, stream, &pkg_node);
    }

    pub fn write_file(self, ns_name: Symbol, file_name: impl AsRef<Path>) {
        let mut stream = String::default();
        self.write_items(
            &mut stream,
            self.codegen_items.iter().map(|def_id| (*def_id).into()),
        );

        let doc = self.doc_header.as_str();
        stream = format! {r#"{doc}
        #![allow(warnings, clippy::all)]
                {stream}
        "#};

        // stream = format! {r#"pub mod {ns_name} {{
        //         #![allow(warnings, clippy::all)]
        //         {stream}
        //     }}"#};

        let mut file = std::io::BufWriter::new(std::fs::File::create(&file_name).unwrap());
        file.write_all(stream.to_string().as_bytes()).unwrap();
        file.flush().unwrap();
        fmt_file(file_name)
    }

    pub fn gen(self) -> anyhow::Result<()> {
        match &*self.mode.clone() {
            Mode::Workspace(info) => self.write_workspace(info.dir.clone()),
            Mode::SingleFile { file_path: p } => {
                self.write_file(
                    FastStr::new(
                        p.file_name()
                            .and_then(|s| s.to_str())
                            .and_then(|s| s.split('.').next())
                            .unwrap(),
                    )
                        .into(),
                    p,
                );
                Ok(())
            }
        }
    }
}
