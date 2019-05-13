use crate::{
    analysis::{
        c_type::{implements_c_type, rustify_pointers},
        namespaces,
        rust_type::{Result, TypeError},
    },
    env::Env,
    library::{self, *},
    traits::*,
};
use log::{info, trace, warn};

// FIXME: This module needs redundant allocations audit
// TODO: ffi_type computations should be cached

pub fn ffi_type(env: &Env, tid: library::TypeId, c_type: &str) -> Result {
    let (ptr, inner) = rustify_pointers(c_type);
    let res = if ptr.is_empty() {
        if let Some(c_tid) = env.library.find_type(0, c_type) {
            // Fast track plain fundamental types avoiding some checks
            if env
                .library
                .type_(c_tid)
                .maybe_ref_as::<Fundamental>()
                .is_some()
            {
                match *env.library.type_(tid) {
                    Type::FixedArray(inner_tid, size, ref inner_c_type) => {
                        let inner_c_type = inner_c_type
                            .as_ref()
                            .map(String::as_str)
                            .unwrap_or_else(|| c_type);
                        ffi_type(env, inner_tid, inner_c_type)
                            .map_any(|s| format!("[{}; {}]", s, size))
                    }
                    Type::Class(Class {
                        c_type: ref expected,
                        ..
                    })
                    | Type::Interface(Interface {
                        c_type: ref expected,
                        ..
                    }) if c_type == "gpointer" => {
                        info!("[c:type `gpointer` instead of `*mut {}`, fixing]", expected);
                        ffi_inner(env, tid, expected.clone()).map_any(|s| format!("*mut {}", s))
                    }
                    _ => ffi_inner(env, c_tid, c_type.into()),
                }
            } else {
                // c_type isn't fundamental
                ffi_inner(env, tid, inner)
            }
        } else {
            // c_type doesn't match any type in the library by name
            ffi_inner(env, tid, inner)
        }
    } else {
        // ptr not empty
        ffi_inner(env, tid, inner).map_any(|s| format!("{} {}", ptr, s))
    };
    trace!("ffi_type({:?}, {}) -> {:?}", tid, c_type, res);
    res
}

fn ffi_inner(env: &Env, tid: library::TypeId, mut inner: String) -> Result {
    let volatile = inner.starts_with("volatile ");
    if volatile {
        inner = inner["volatile ".len()..].into();
    }

    let typ = env.library.type_(tid);
    let res = match *typ {
        Type::Fundamental(fund) => {
            use crate::library::Fundamental::*;
            let inner = match fund {
                None => "c_void",
                Boolean => "gboolean",
                Int8 => "i8",
                UInt8 => "u8",
                Int16 => "i16",
                UInt16 => "u16",
                Int32 => "i32",
                UInt32 => "u32",
                Int64 => "i64",
                UInt64 => "u64",
                Char => "c_char",
                UChar => "c_uchar",
                Short => "c_short",
                UShort => "c_ushort",
                Int => "c_int",
                UInt => "c_uint",
                Long => "c_long",
                ULong => "c_ulong",
                Size => "size_t",
                SSize => "ssize_t",
                Float => "c_float",
                Double => "c_double",
                UniChar => "u32",
                Utf8 => "c_char",
                Filename => "c_char",
                OsString => "c_char",
                Type => "GType",
                Pointer => {
                    match &inner[..] {
                        "void" => "c_void",
                        "tm" => return Err(TypeError::Unimplemented(inner)), //TODO: try use time:Tm
                        _ => &*inner,
                    }
                }
                IntPtr => "intptr_t",
                UIntPtr => "uintptr_t",
                Unsupported => return Err(TypeError::Unimplemented(inner)),
                VarArgs => panic!("Should not reach here"),
            };
            Ok(inner.into())
        }
        Type::Record(..) | Type::Alias(..) | Type::Function(..) => {
            if let Some(declared_c_type) = typ.get_glib_name() {
                if declared_c_type != inner {
                    let msg = format!(
                        "[c:type mismatch `{}` != `{}` of `{}`]",
                        inner,
                        declared_c_type,
                        typ.get_name()
                    );
                    warn!("{}", msg);
                    return Err(TypeError::Mismatch(msg));
                }
            } else {
                warn!("Type `{}` missing c_type", typ.get_name());
            }
            fix_name(env, tid, &inner)
        }
        Type::CArray(inner_tid) => ffi_inner(env, inner_tid, inner),
        Type::FixedArray(inner_tid, size, ref inner_c_type) => {
            let inner_c_type = inner_c_type
                .as_ref()
                .map(String::as_str)
                .unwrap_or_else(|| inner.as_str());
            ffi_type(env, inner_tid, inner_c_type).map_any(|s| format!("[{}; {}]", s, size))
        }
        Type::Array(..)
        | Type::PtrArray(..)
        | Type::List(..)
        | Type::SList(..)
        | Type::HashTable(..) => fix_name(env, tid, &inner),
        _ => {
            if let Some(glib_name) = env.library.type_(tid).get_glib_name() {
                if inner != glib_name {
                    if inner == "gpointer" {
                        fix_name(env, tid, glib_name).map_any(|s| format!("*mut {}", s))
                    } else if implements_c_type(env, tid, &inner) {
                        info!(
                            "[c:type {} of {} <: {}, fixing]",
                            glib_name,
                            env.library.type_(tid).get_name(),
                            inner
                        );
                        fix_name(env, tid, glib_name)
                    } else {
                        let msg = format!(
                            "[c:type mismatch {} != {} of {}]",
                            inner,
                            glib_name,
                            env.library.type_(tid).get_name()
                        );
                        warn!("{}", msg);
                        Err(TypeError::Mismatch(msg))
                    }
                } else {
                    fix_name(env, tid, &inner)
                }
            } else {
                let msg = format!(
                    "[Missing glib_name of {}, can't match != {}]",
                    env.library.type_(tid).get_name(),
                    inner
                );
                warn!("{}", msg);
                Err(TypeError::Mismatch(msg))
            }
        }
    };

    if volatile {
        res.map(|s| format!("/*volatile*/{}", s))
    } else {
        res
    }
}

fn fix_name(env: &Env, type_id: library::TypeId, name: &str) -> Result {
    if type_id.ns_id == library::INTERNAL_NAMESPACE {
        match *env.library.type_(type_id) {
            Type::Array(..)
            | Type::PtrArray(..)
            | Type::List(..)
            | Type::SList(..)
            | Type::HashTable(..) => {
                if env.namespaces.glib_ns_id == namespaces::MAIN {
                    Ok(name.into())
                } else {
                    Ok(format!(
                        "{}::{}",
                        &env.namespaces[env.namespaces.glib_ns_id].crate_name, name
                    ))
                }
            }
            _ => Ok(name.into()),
        }
    } else {
        let name_with_prefix = if type_id.ns_id == namespaces::MAIN {
            name.into()
        } else {
            format!("{}::{}", &env.namespaces[type_id.ns_id].crate_name, name)
        };
        if env
            .type_status_sys(&type_id.full_name(&env.library))
            .ignored()
        {
            Err(TypeError::Ignored(name_with_prefix))
        } else {
            Ok(name_with_prefix)
        }
    }
}
