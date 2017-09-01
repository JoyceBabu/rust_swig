use std::collections::HashMap;

use syntex_syntax::symbol::Symbol;
use syntex_syntax::parse::{PResult, ParseSess};
use syntex_syntax::ptr::P;
use syntex_syntax::ast;
use syntex_syntax::ast::DUMMY_NODE_ID;
use syntex_pos::DUMMY_SP;
use syntex_syntax::print::pprust;

use {ForeignEnumInfo, ForeignInterface, ForeignInterfaceMethod, ForeignerClassInfo,
     ForeignerMethod, MethodVariant, SelfTypeVariant, TypesConvMap};
use super::{code_to_item, fmt_write_err_map, java_class_full_name, java_class_name_to_jni,
            method_name, ForeignMethodSignature, ForeignTypeInfo};
use errors::fatal_error;
use my_ast::{normalized_ty_string, parse_ty, self_variant, RustType};
use types_conv_map::{unpack_unique_typename, FROM_VAR_TEMPLATE, TO_VAR_TEMPLATE};

struct MethodContext<'a> {
    method: &'a ForeignerMethod,
    f_method: &'a ForeignMethodSignature,
    jni_func_name: &'a str,
    decl_func_args: &'a str,
    args_names: &'a str,
    real_output_typename: &'a str,
}

pub(in java_jni) fn generate_rust_code<'a>(
    sess: &'a ParseSess,
    conv_map: &mut TypesConvMap,
    package_name: &str,
    class: &ForeignerClassInfo,
    f_methods_sign: &[ForeignMethodSignature],
) -> PResult<'a, Vec<P<ast::Item>>> {


    //to handle java method overload
    let mut gen_fnames = HashMap::<String, usize>::new();
    for (method, f_method) in class.methods.iter().zip(f_methods_sign.iter()) {
        let val_ref = gen_fnames.entry(method_name(method, f_method));
        *val_ref.or_insert(0) += 1;
    }

    let dummy_ty = ast::Ty {
        id: DUMMY_NODE_ID,
        node: ast::TyKind::Tup(vec![]),
        span: DUMMY_SP,
    };
    let mut gen_code = Vec::<P<ast::Item>>::new();
    let (this_type_for_method, code_box_this) = if let (
        Some(this_type),
        Some(constructor_ret_type),
    ) = (
        class.this_type_for_method.as_ref(),
        class.constructor_ret_type.as_ref(),
    ) {
        let this_type: RustType = this_type.clone().into();
        let this_type = this_type.implements("SwigForeignClass");
        debug!(
            "generate_rust_code: add implements SwigForeignClass for {}",
            this_type.normalized_name
        );
        conv_map.add_type(this_type.clone());

        let constructor_ret_type: RustType = constructor_ret_type.clone().into();
        conv_map.add_type(constructor_ret_type);

        let (this_type_for_method, code_box_this) =
            TypesConvMap::convert_to_heap_pointer(&this_type, "this");
        let class_name_for_user = java_class_full_name(package_name, &class.name.as_str());
        let class_name_for_jni = java_class_name_to_jni(&class_name_for_user);

        gen_code.append(&mut code_to_item(
            sess,
            &class_name_for_jni,
            &format!(
                r#"impl SwigForeignClass for {class_name} {{
    fn jni_class_name() -> *const ::std::os::raw::c_char {{
        swig_c_str!("{jni_class_name}")
    }}
    fn box_object(this: Self) -> jlong {{
{code_box_this}
       this as jlong
    }}
}}"#,
                class_name = this_type.normalized_name,
                jni_class_name = class_name_for_jni,
                code_box_this = code_box_this,
            ),
        )?);


        let jlong_ti: RustType = parse_ty(sess, DUMMY_SP, Symbol::intern("jlong"))?.into();

        conv_map.add_conversation_rule(
            jlong_ti.clone(),
            get_ref_type(&this_type_for_method.ty, ast::Mutability::Immutable).into(),
            Symbol::intern(&format!(
                r#"
    let {to_var}: &{this_type} = unsafe {{
        jlong_to_pointer::<{this_type}>({from_var}).as_mut().unwrap()
    }};
"#,
                to_var = TO_VAR_TEMPLATE,
                from_var = FROM_VAR_TEMPLATE,
                this_type = this_type_for_method.normalized_name,
            )).into(),
        );

        conv_map.add_conversation_rule(
            jlong_ti.clone(),
            get_ref_type(&this_type_for_method.ty, ast::Mutability::Mutable).into(),
            Symbol::intern(&format!(
                r#"
    let {to_var}: &mut {this_type} = unsafe {{
        jlong_to_pointer::<{this_type}>({from_var}).as_mut().unwrap()
    }};
"#,
                to_var = TO_VAR_TEMPLATE,
                from_var = FROM_VAR_TEMPLATE,
                this_type = this_type_for_method.normalized_name,
            )).into(),
        );

        let unpack_code =
            TypesConvMap::unpack_from_heap_pointer(&this_type_for_method, TO_VAR_TEMPLATE, true);
        conv_map.add_conversation_rule(
            jlong_ti,
            this_type,
            Symbol::intern(&format!(
                r#"
    let {to_var}: *mut {this_type} = unsafe {{
        jlong_to_pointer::<{this_type}>({from_var}).as_mut().unwrap()
    }};
{unpack_code}
"#,
                to_var = TO_VAR_TEMPLATE,
                from_var = FROM_VAR_TEMPLATE,
                this_type = this_type_for_method.normalized_name,
                unpack_code = unpack_code,
            )).into(),
        );

        (this_type_for_method, code_box_this)
    } else {
        (dummy_ty.clone().into(), String::new())
    };


    let no_this_info = || {
        fatal_error(
            sess,
            class.span,
            &format!(
                "Class {} (package {}) have methods, but there is no constructor",
                class.name,
                package_name,
            ),
        )
    };

    let mut have_constructor = false;

    for (method, f_method) in class.methods.iter().zip(f_methods_sign.iter()) {
        let java_method_name = method_name(method, f_method);
        let method_overloading = gen_fnames[&java_method_name] > 1;
        let jni_func_name = generate_jni_func_name(
            sess,
            package_name,
            class,
            &java_method_name,
            f_method,
            method_overloading,
        )?;
        trace!("generate_rust_code jni name: {}", jni_func_name);

        let args_names = f_method
            .input
            .iter()
            .enumerate()
            .map(|a| format!("a_{}, ", a.0))
            .fold(String::new(), |acc, x| acc + &x);

        let decl_func_args = generate_jni_args_with_types(f_method)
            .map_err(|err| fatal_error(sess, class.span, &err))?;
        let real_output_typename = match method.fn_decl.output {
            ast::FunctionRetTy::Default(_) => "()".to_string(),
            ast::FunctionRetTy::Ty(ref t) => normalized_ty_string(&*t),
        };

        let method_ctx = MethodContext {
            method,
            f_method,
            jni_func_name: &jni_func_name,
            decl_func_args: &decl_func_args,
            args_names: &args_names,
            real_output_typename: &real_output_typename,
        };

        match method.variant {
            MethodVariant::StaticMethod => {
                gen_code.append(&mut generate_static_method(sess, conv_map, &method_ctx)?);
            }
            MethodVariant::Method(ref self_variant) => {
                gen_code.append(&mut generate_method(
                    sess,
                    conv_map,
                    &method_ctx,
                    class,
                    *self_variant,
                    &this_type_for_method,
                )?);
            }
            MethodVariant::Constructor => {
                have_constructor = true;
                let constructor_ret_type = class
                    .constructor_ret_type
                    .as_ref()
                    .ok_or_else(&no_this_info)?
                    .clone();
                let this_type = class
                    .this_type_for_method
                    .as_ref()
                    .ok_or_else(&no_this_info)?
                    .clone();
                gen_code.append(&mut generate_constructor(
                    sess,
                    conv_map,
                    &method_ctx,
                    constructor_ret_type,
                    this_type,
                    &code_box_this,
                )?);
            }
        }
    }

    if have_constructor {
        let this_type: RustType = class
            .this_type_for_method
            .as_ref()
            .ok_or_else(&no_this_info)?
            .clone()
            .into();
        let unpack_code = TypesConvMap::unpack_from_heap_pointer(&this_type, "this", false);

        let jni_destructor_name = generate_jni_func_name(
            sess,
            package_name,
            class,
            "do_delete",
            &ForeignMethodSignature {
                output: ForeignTypeInfo {
                    name: Symbol::intern(""),
                    correspoding_rust_type: dummy_ty.into(),
                },
                input: vec![],
            },
            false,
        )?;
        let code = format!(
            r#"
#[allow(unused_variables, unused_mut, non_snake_case)]
#[no_mangle]
pub fn {jni_destructor_name}(env: *mut JNIEnv, _: jclass, this: jlong) {{
    let this: *mut {this_type} = unsafe {{
        jlong_to_pointer::<{this_type}>(this).as_mut().unwrap()
    }};
{unpack_code}
    drop(this);
}}
"#,
            jni_destructor_name = jni_destructor_name,
            unpack_code = unpack_code,
            this_type = this_type_for_method.normalized_name,
        );
        debug!("we generate and parse code: {}", code);
        gen_code.append(&mut code_to_item(sess, &jni_destructor_name, &code)?);
    }

    Ok(gen_code)
}

pub(in java_jni) fn generate_rust_code_for_enum<'a>(
    sess: &'a ParseSess,
    conv_map: &mut TypesConvMap,
    enum_info: &ForeignEnumInfo,
) -> PResult<'a, Vec<P<ast::Item>>> {
    use std::fmt::Write;

    let rust_enum_name = enum_info.rust_enum_name();
    let mut code = format!(
        r#"
impl SwigFrom<jint> for {rust_enum_name} {{
    fn swig_from(x: jint, _: *mut JNIEnv) -> {rust_enum_name} {{
        match x {{

"#,
        rust_enum_name = rust_enum_name,
    );
    for (i, item) in enum_info.items.iter().enumerate() {
        write!(
            &mut code,
            "{index} => {item_name},\n",
            index = i,
            item_name = item.rust_name
        ).unwrap();
    }
    write!(
        &mut code,
        r#"
        _ => panic!("{{}} not expected for {rust_enum_name}", x),
        }}
    }}
}}
"#,
        rust_enum_name = rust_enum_name,
    ).unwrap();
    conv_map.register_exported_enum(enum_info);
    conv_map
        .merge(sess, &*enum_info.rust_enum_name().as_str(), &code)?;
    Ok(vec![])
}

pub(in java_jni) fn generate_interface<'a>(
    sess: &'a ParseSess,
    conv_map: &mut TypesConvMap,
    interface: &ForeignInterface,
    methods_sign: &[ForeignMethodSignature],
) -> PResult<'a, Vec<P<ast::Item>>> {
    use std::fmt::Write;

    let mut new_conv_code = format!(
        r#"
#[swig_from_foreigner_hint = "{interface_name}"]
impl SwigFrom<jobject> for Box<{trait_name}> {{
    fn swig_from(this: jobject, env: *mut JNIEnv) -> Self {{
        let mut cb = JavaCallback::new(this, env);
        cb.methods.reserve({methods_len});
        let class = unsafe {{ (**env).GetObjectClass.unwrap()(env, cb.this) }};
        assert!(!class.is_null(), "GetObjectClass return null class for {interface_name}");
"#,
        interface_name = interface.name,
        trait_name = interface.self_type,
        methods_len = interface.items.len(),
    );
    for (method, f_method) in interface.items.iter().zip(methods_sign) {
        write!(
            &mut new_conv_code,
            r#"
        let method_id: jmethodID = unsafe {{
            (**env).GetMethodID.unwrap()(env, class, swig_c_str!("{method_name}"),
                                         swig_c_str!("{method_sig}"))
        }};
        assert!(!method_id.is_null(), "Can not find {method_name} id");
        cb.methods.push(method_id);
"#,
            method_name = method.name,
            method_sig = jni_method_signature(f_method),
        ).unwrap();
    }
    write!(
        &mut new_conv_code,
        r#"
        Box::new(cb)
    }}
}}
"#
    ).unwrap();
    conv_map
        .merge(sess, &format!("{}", interface.self_type), &new_conv_code)?;

    let mut gen_items = vec![];

    let mut impl_trait_code = format!(
        r#"
impl {trait_name} for JavaCallback {{
"#,
        trait_name = interface.self_type
    );

    for (method_idx, (method, f_method)) in interface.items.iter().zip(methods_sign).enumerate() {
        let func_name = method
            .rust_name
            .segments
            .last()
            .ok_or_else(|| {
                fatal_error(sess, method.rust_name.span, "Empty trait function name")
            })?
            .identifier
            .name;
        let rest_args_with_types: String = method
            .fn_decl
            .inputs
            .iter()
            .skip(1)
            .enumerate()
            .map(|(i, v)| {
                format!("a_{}: {}", i, pprust::ty_to_string(&*v.ty))
            })
            .fold(String::new(), |mut acc, x| {
                acc.push_str(", ");
                acc.push_str(&x);
                acc
            });
        let self_arg = match self_variant(&method.fn_decl.inputs[0].ty)
            .expect("Expect Self type for first argument")
        {
            SelfTypeVariant::Default => "self",
            SelfTypeVariant::Mut => "mut self",
            SelfTypeVariant::Rptr => "&self",
            SelfTypeVariant::RptrMut => "&mut self",
        };
        let args_with_types: String = [self_arg.to_string(), rest_args_with_types].concat();
        assert!(!method.fn_decl.inputs.is_empty());
        let n_args = method.fn_decl.inputs.len() - 1;
        let args: String = (0..n_args).map(|i| format!(", a_{}", i)).fold(
            String::new(),
            |mut acc, x| {
                acc.push_str(&x);
                acc
            },
        );
        let (mut conv_deps, convert_args) = rust_to_foreign_convert_method_inputs(
            sess,
            conv_map,
            method,
            f_method,
            (0..n_args).map(|v| format!("a_{}", v)),
            "()",
        )?;

        write!(
            &mut impl_trait_code,
            r#"
    #[allow(unused_mut)]
    fn {func_name}({args_with_types}) {{
        let env = self.get_jni_env();
        if let Some(env) = env.env {{
{convert_args}  
            unsafe {{
                (**env).CallVoidMethod.unwrap()(env, self.this, self.methods[{method_idx}]
                                                {args});
                if (**env).ExceptionCheck.unwrap()(env) != 0 {{
                    error!("{func_name}: java throw exception");
                    (**env).ExceptionDescribe.unwrap()(env);
                    (**env).ExceptionClear.unwrap()(env);
                }}   
            }};
        }}
    }}
"#,
            func_name = func_name,
            args_with_types = args_with_types,
            method_idx = method_idx,
            args = args,
            convert_args = convert_args,
        ).unwrap();
        gen_items.append(&mut conv_deps);
    }

    write!(
        &mut impl_trait_code,
        r#"
}}
"#
    ).unwrap();
    gen_items.append(&mut code_to_item(
        sess,
        &format!("impl {} for JavaCallback", interface.self_type),
        &impl_trait_code,
    )?);
    Ok(gen_items)
}

lazy_static! {
    static ref JAVA_TYPE_NAMES_FOR_JNI_SIGNATURE: HashMap<Symbol, &'static str> = {
        let mut m = HashMap::new();
        m.insert(Symbol::intern("String"), "Ljava.lang.String;");
        m.insert(Symbol::intern("boolean"), "Z");
        m.insert(Symbol::intern("byte"), "B");
        m.insert(Symbol::intern("char"), "C");
        m.insert(Symbol::intern("double"), "D");
        m.insert(Symbol::intern("float"), "F");
        m.insert(Symbol::intern("int"), "I");
        m.insert(Symbol::intern("long"), "J");
        m.insert(Symbol::intern("object"), "L");
        m.insert(Symbol::intern("short"), "S");
        m.insert(Symbol::intern("void"), "V");
        m
    };
}

fn generate_jni_func_name<'a>(
    sess: &'a ParseSess,
    package_name: &str,
    class: &ForeignerClassInfo,
    java_method_name: &str,
    f_method: &ForeignMethodSignature,
    overloaded: bool,
) -> PResult<'a, String> {
    let mut output = String::new();
    output.push_str("Java_");
    fn escape_underscore(input: &str, output: &mut String) {
        for c in input.chars() {
            match c {
                '.' => output.push('_'),
                '[' => output.push_str("_3"),
                '_' => output.push_str("_1"),
                ';' => output.push_str("_2"),
                _ => output.push(c),
            }
        }
    }
    escape_underscore(package_name, &mut output);
    output.push_str("_");
    escape_underscore(&class.name.as_str(), &mut output);
    output.push_str("_");
    escape_underscore(java_method_name, &mut output);

    if overloaded {
        output.push_str("__");
        for arg in &f_method.input {
            let type_name = if arg.java_need_conversation() {
                arg.java_transition_type.unwrap()
            } else {
                arg.name
            };
            let type_name = JAVA_TYPE_NAMES_FOR_JNI_SIGNATURE
                .get(&type_name)
                .ok_or_else(|| {
                    fatal_error(
                        sess,
                        class.span,
                        &format!(
                            "Can not generate JNI function name for overload method '{}',\
                             unknown java type '{}'",
                            java_method_name,
                            arg.name
                        ),
                    )
                })?;

            escape_underscore(type_name, &mut output);
        }
    }

    Ok(output)
}

fn foreign_from_rust_convert_method_output<'a>(
    sess: &'a ParseSess,
    conv_map: &mut TypesConvMap,
    rust_ret_ty: &ast::FunctionRetTy,
    f_output: &ForeignTypeInfo,
    var_name: &str,
    func_ret_type: &str,
) -> PResult<'a, (Vec<P<ast::Item>>, String)> {
    let rust_ret_ty: ast::Ty = match *rust_ret_ty {
        ast::FunctionRetTy::Default(ref span) => if &*(f_output.name.as_str()) != "void" {
            return Err(fatal_error(
                sess,
                *span,
                &format!("Rust type `()` mapped to not void ({})", f_output.name),
            ));
        } else {
            return Ok((Vec::new(), String::new()));
        },
        ast::FunctionRetTy::Ty(ref p_ty) => (**p_ty).clone(),
    };
    let context_span = rust_ret_ty.span;
    conv_map.convert_rust_types(
        sess,
        &rust_ret_ty.into(),
        &f_output.correspoding_rust_type,
        var_name,
        func_ret_type,
        context_span,
    )
}

fn foreign_to_rust_convert_method_inputs<'a, GI: Iterator<Item = String>>(
    sess: &'a ParseSess,
    conv_map: &mut TypesConvMap,
    method: &ForeignerMethod,
    f_method: &ForeignMethodSignature,
    arg_names: GI,
    func_ret_type: &str,
) -> PResult<'a, (Vec<P<ast::Item>>, String)> {
    let mut code_deps = Vec::<P<ast::Item>>::new();
    let mut ret_code = String::new();

    //skip self
    let skip_n = match method.variant {
        MethodVariant::Method(_) => 1,
        _ => 0,
    };
    for ((to_type, f_from), arg_name) in method
        .fn_decl
        .inputs
        .iter()
        .skip(skip_n)
        .zip(f_method.input.iter())
        .zip(arg_names)
    {
        let to: RustType = (*to_type.ty).clone().into();
        let (mut cur_deps, cur_code) = conv_map.convert_rust_types(
            sess,
            &f_from.correspoding_rust_type,
            &to,
            &arg_name,
            func_ret_type,
            to_type.pat.span,
        )?;
        code_deps.append(&mut cur_deps);
        ret_code.push_str(&cur_code);
    }
    Ok((code_deps, ret_code))
}

fn rust_to_foreign_convert_method_inputs<'a, GI: Iterator<Item = String>>(
    sess: &'a ParseSess,
    conv_map: &mut TypesConvMap,
    method: &ForeignInterfaceMethod,
    f_method: &ForeignMethodSignature,
    arg_names: GI,
    func_ret_type: &str,
) -> PResult<'a, (Vec<P<ast::Item>>, String)> {
    let mut code_deps = Vec::<P<ast::Item>>::new();
    let mut ret_code = String::new();


    for ((from_ty, to_f), arg_name) in method
        .fn_decl
        .inputs
        .iter()
        .skip(1)//skip self
        .zip(f_method.input.iter())
        .zip(arg_names)
    {
        let from: RustType = (*from_ty.ty).clone().into();
        let (mut cur_deps, cur_code) = conv_map.convert_rust_types(
            sess,
            &from,
            &to_f.correspoding_rust_type,
            &arg_name,
            func_ret_type,
            from_ty.pat.span,
        )?;
        code_deps.append(&mut cur_deps);
        ret_code.push_str(&cur_code);
    }
    Ok((code_deps, ret_code))
}

fn generate_jni_args_with_types(f_method: &ForeignMethodSignature) -> Result<String, String> {
    use std::fmt::Write;

    let mut buf = String::new();
    for (i, f_type_info) in f_method.input.iter().enumerate() {
        write!(
            &mut buf,
            "a_{}: {}, ",
            i,
            unpack_unique_typename(f_type_info.correspoding_rust_type.normalized_name)
        ).map_err(fmt_write_err_map)?;
    }
    Ok(buf)
}

fn create_suitable_types_for_constructor_and_self(
    self_variant: SelfTypeVariant,
    class: &ForeignerClassInfo,
    constructor_real_type: &ast::Ty,
) -> (ast::Ty, ast::Ty) {
    match self_variant {
        SelfTypeVariant::Default => {
            unimplemented!();
        }
        SelfTypeVariant::Mut => {
            unimplemented!();
        }
        SelfTypeVariant::Rptr | SelfTypeVariant::RptrMut => {
            let mutbl = if self_variant == SelfTypeVariant::Rptr {
                ast::Mutability::Immutable
            } else {
                ast::Mutability::Mutable
            };
            (
                ast::Ty {
                    id: DUMMY_NODE_ID,
                    span: constructor_real_type.span,
                    node: ast::TyKind::Rptr(
                        None,
                        ast::MutTy {
                            mutbl: mutbl,
                            ty: P(constructor_real_type.clone()),
                        },
                    ),
                },

                ast::Ty {
                    id: DUMMY_NODE_ID,
                    span: class.self_type.span,
                    node: ast::TyKind::Rptr(
                        None,
                        ast::MutTy {
                            mutbl: mutbl,
                            ty: P(ast::Ty {
                                id: DUMMY_NODE_ID,
                                span: class.self_type.span,
                                node: ast::TyKind::Path(None, class.self_type.clone()),
                            }),
                        },
                    ),
                },
            )
        }
    }
}

fn generate_static_method<'a>(
    sess: &'a ParseSess,
    conv_map: &mut TypesConvMap,
    mc: &MethodContext,
) -> PResult<'a, Vec<P<ast::Item>>> {
    let jni_ret_type =
        unpack_unique_typename(mc.f_method.output.correspoding_rust_type.normalized_name);
    let (mut deps_code_out, convert_output_code) = foreign_from_rust_convert_method_output(
        sess,
        conv_map,
        &mc.method.fn_decl.output,
        &mc.f_method.output,
        "ret",
        &jni_ret_type.as_str(),
    )?;
    let n_args = mc.f_method.input.len();
    let (deps_code_in, convert_input_code) = foreign_to_rust_convert_method_inputs(
        sess,
        conv_map,
        mc.method,
        mc.f_method,
        (0..n_args).map(|v| format!("a_{}", v)),
        &jni_ret_type.as_str(),
    )?;

    let code = format!(
        r#"
#[allow(non_snake_case, unused_variables, unused_mut)]
#[no_mangle]
pub fn {func_name}(env: *mut JNIEnv, _: jclass, {decl_func_args}) -> {jni_ret_type} {{
{convert_input_code}
    let mut ret: {real_output_typename} = {rust_func_name}({args_names});
{convert_output_code}
    ret
}}
"#,
        func_name = mc.jni_func_name,
        decl_func_args = mc.decl_func_args,
        jni_ret_type = jni_ret_type,
        convert_input_code = convert_input_code,
        rust_func_name = mc.method.rust_id,
        args_names = mc.args_names,
        convert_output_code = convert_output_code,
        real_output_typename = mc.real_output_typename,
    );
    let mut gen_code = deps_code_in;
    gen_code.append(&mut deps_code_out);
    gen_code.append(&mut code_to_item(sess, mc.jni_func_name, &code)?);
    Ok(gen_code)
}

fn generate_constructor<'a>(
    sess: &'a ParseSess,
    conv_map: &mut TypesConvMap,
    mc: &MethodContext,
    construct_ret_type: ast::Ty,
    this_type: ast::Ty,
    code_box_this: &str,
) -> PResult<'a, Vec<P<ast::Item>>> {
    let n_args = mc.f_method.input.len();
    let (deps_code_in, convert_input_code) = foreign_to_rust_convert_method_inputs(
        sess,
        conv_map,
        mc.method,
        mc.f_method,
        (0..n_args).map(|v| format!("a_{}", v)),
        "jlong",
    )?;

    let this_type: RustType = this_type.into();

    let (mut deps_this, convert_this) = conv_map.convert_rust_types(
        sess,
        &construct_ret_type.into(),
        &this_type,
        "this",
        "jlong",
        mc.method.span(),
    )?;


    let code = format!(
        r#"
#[no_mangle]
#[allow(unused_variables, unused_mut, non_snake_case)]
pub fn {func_name}(env: *mut JNIEnv, _: jclass, {decl_func_args}) -> jlong {{
{convert_input_code}
    let this: {real_output_typename} = {rust_func_name}({args_names});
{convert_this}
{box_this}
    this as jlong
}}
"#,
        func_name = mc.jni_func_name,
        convert_this = convert_this,
        decl_func_args = mc.decl_func_args,
        convert_input_code = convert_input_code,
        rust_func_name = mc.method.rust_id,
        args_names = mc.args_names,
        box_this = code_box_this,
        real_output_typename = mc.real_output_typename,
    );
    let mut gen_code = deps_code_in;
    gen_code.append(&mut deps_this);
    gen_code.append(&mut code_to_item(sess, mc.jni_func_name, &code)?);
    Ok(gen_code)
}

fn generate_method<'a>(
    sess: &'a ParseSess,
    conv_map: &mut TypesConvMap,
    mc: &MethodContext,
    class: &ForeignerClassInfo,
    self_variant: SelfTypeVariant,
    this_type_for_method: &RustType,
) -> PResult<'a, Vec<P<ast::Item>>> {
    let jni_ret_type =
        unpack_unique_typename(mc.f_method.output.correspoding_rust_type.normalized_name);
    let n_args = mc.f_method.input.len();
    let (deps_code_in, convert_input_code) = foreign_to_rust_convert_method_inputs(
        sess,
        conv_map,
        mc.method,
        mc.f_method,
        (0..n_args).map(|v| format!("a_{}", v)),
        &jni_ret_type.as_str(),
    )?;

    let (mut deps_code_out, convert_output_code) = foreign_from_rust_convert_method_output(
        sess,
        conv_map,
        &mc.method.fn_decl.output,
        &mc.f_method.output,
        "ret",
        &jni_ret_type.as_str(),
    )?;

    //&mut constructor_real_type -> &mut class.self_type

    let (from_ty, to_ty): (ast::Ty, ast::Ty) = create_suitable_types_for_constructor_and_self(
        self_variant,
        class,
        &this_type_for_method.ty,
    );
    let this_type_ref = normalized_ty_string(&from_ty);
    let (mut deps_this, convert_this) = conv_map.convert_rust_types(
        sess,
        &from_ty.into(),
        &to_ty.into(),
        "this",
        &jni_ret_type.as_str(),
        mc.method.span(),
    )?;

    let code = format!(
        r#"
#[allow(non_snake_case, unused_variables, unused_mut)]
#[no_mangle]
pub fn {func_name}(env: *mut JNIEnv, _: jclass, this: jlong, {decl_func_args}) -> {jni_ret_type} {{
{convert_input_code}
    let this: {this_type_ref} = unsafe {{
        jlong_to_pointer::<{this_type}>(this).as_mut().unwrap()
    }};
{convert_this}
    let mut ret: {real_output_typename} = {rust_func_name}(this, {args_names});
{convert_output_code}
    ret
}}
"#,
        func_name = mc.jni_func_name,
        decl_func_args = mc.decl_func_args,
        convert_input_code = convert_input_code,
        jni_ret_type = jni_ret_type,
        this_type_ref = this_type_ref,
        this_type = this_type_for_method.normalized_name,
        convert_this = convert_this,
        rust_func_name = mc.method.rust_id,
        args_names = mc.args_names,
        convert_output_code = convert_output_code,
        real_output_typename = mc.real_output_typename,
    );
    let mut gen_code = deps_code_in;
    gen_code.append(&mut deps_code_out);
    gen_code.append(&mut deps_this);
    gen_code.append(&mut code_to_item(sess, mc.jni_func_name, &code)?);
    Ok(gen_code)
}

fn get_ref_type(ty: &ast::Ty, mutbl: ast::Mutability) -> ast::Ty {
    ast::Ty {
        id: DUMMY_NODE_ID,
        span: ty.span,
        node: ast::TyKind::Rptr(
            None,
            ast::MutTy {
                mutbl: mutbl,
                ty: P(ty.clone()),
            },
        ),
    }
}

fn jni_method_signature(method: &ForeignMethodSignature) -> String {
    let mut ret: String = "(".into();
    for arg in &method.input {
        let sig = JAVA_TYPE_NAMES_FOR_JNI_SIGNATURE
            .get(&arg.name)
            .unwrap_or_else(|| {
                panic!("Unknown type `{}`, can not generate jni signature")
            });
        ret.push_str(sig);
    }
    ret.push(')');
    let sig = JAVA_TYPE_NAMES_FOR_JNI_SIGNATURE
        .get(&method.output.name)
        .unwrap_or_else(|| {
            panic!("Unknown type `{}`, can not generate jni signature")
        });
    ret.push_str(sig);
    ret
}
