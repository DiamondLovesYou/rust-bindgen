use core::hashmap::linear::LinearMap;
use core::io::WriterUtil;
use core::to_bytes;
use core::libc::*;

use types::*;
use clang::*;
use gen::*;

struct BindGenCtx {
    match_pat: ~[~str],
    link: Option<~str>,
    out: @io::Writer,
    name: LinearMap<CXCursor, Global>,
    globals: ~[Global],
    cur_glob: Global
}

enum ParseResult {
    CmdUsage,
    ParseOk(~[~str], @mut BindGenCtx),
    ParseErr(~str)
}

impl Eq for CXCursor {
    fn eq(&self, other: &CXCursor) -> bool {
        return (self.kind as int == other.kind as int) &&
               (self.xdata as int == other.xdata as int) &&
               (self.data[0] as int == other.data[0] as int) &&
               (self.data[1] as int == other.data[1] as int) &&
               (self.data[2] as int == other.data[2] as int)
    }

    fn ne(&self, other: &CXCursor) -> bool {
        return !self.eq(other);
    }
}

impl IterBytes for CXCursor {
    fn iter_bytes(&self, lsb0: bool, f: to_bytes::Cb) {
        to_bytes::iter_bytes_5(
            &(self.kind as int),
            &(self.xdata as int),
            &(self.data[0] as int),
            &(self.data[1] as int),
            &(self.data[2] as int),
            lsb0, f
        );
    }
}

impl ToStr for CXString {
    fn to_str(&self) -> ~str {
        unsafe {
            return str::raw::from_c_str(clang_getCString(*self));
        }
    }
}

fn parse_args(args: &[~str]) -> ParseResult {
    let mut clang_args = ~[];
    let args_len = vec::len(args);

    let mut out = io::stdout();
    let mut pat = ~[];
    let mut link = None;

    if args_len == 0u {
        return CmdUsage;
    }

    let mut ix = 0u;
    while ix < args_len {
        match args[ix] {
            ~"--help" | ~"-h" => {
                return CmdUsage;
            }
            ~"-o" => {
                if ix + 1u > args_len {
                    return ParseErr(~"Missing output filename");
                }
                match io::file_writer(&path::Path(args[ix + 1u]),
                                      ~[io::Create, io::Truncate]) {
                    result::Ok(f) => { out = f; }
                    result::Err(e) => { return ParseErr(e); }
                }
                ix += 2u;
            }
            ~"-l" => {
                if ix + 1u > args_len {
                    return ParseErr(~"Missing link name");
                }
                link = Some(copy args[ix + 1u]);
                ix += 2u;
            }
            ~"-match" => {
                if ix + 1u > args_len {
                    return ParseErr(~"Missing match pattern");
                }
                pat.push(copy args[ix + 1u]);
                ix += 2u;
            }
            _ => {
                clang_args.push(copy args[ix]);
                ix += 1u;
            }
        }
    }

    let ctx = @mut BindGenCtx { match_pat: pat,
                                link: link,
                                out: out,
                                name: LinearMap::new(),
                                globals: ~[],
                                cur_glob: GOther
                              };

    return ParseOk(clang_args, ctx);
}

fn print_usage(bin: ~str) {
    io::print(fmt!("Usage: %s [options] input.h", bin) +
"
Options:
    -h or --help    Display help message
    -l <name>       Link name of the library
    -o <output.rs>  Write bindings to <output.rs> (default stdout)
    -match <name>   Only output bindings for definitions from files
                    whose name contains <name>
                    If multiple -match options are provided, files
                    matching any rule are bound to.

    Options other than stated above are passed to clang.
"
    );
}

unsafe fn match_pattern(ctx: @mut BindGenCtx, cursor: CXCursor) -> bool {
    let file = ptr::null();
    clang_getSpellingLocation(clang_getCursorLocation(cursor),
                              ptr::to_unsafe_ptr(&file),
                              ptr::null(), ptr::null(), ptr::null());

    if file as int == 0 {
        return false;
    }

    if vec::is_empty(ctx.match_pat) {
        return true;
    }

    let name = clang_getFileName(file).to_str();
    for ctx.match_pat.each |pat| {
        if str::contains(name, *pat) {
            return true;
        }
    }

    return false;
}

unsafe fn decl_name(ctx: @mut BindGenCtx, cursor: CXCursor) -> Global {
    let decl_opt = ctx.name.find(&cursor);
    match decl_opt {
        option::Some(decl) => { return *decl; }
        None => {
            let spelling = clang_getCursorSpelling(cursor).to_str();

            let decl = if cursor.kind == CXCursor_StructDecl {
                let ci = mk_compinfo(spelling, true);
                GCompDecl(ci)
            } else if cursor.kind == CXCursor_UnionDecl {
                let ci = mk_compinfo(spelling, false);
                GCompDecl(ci)
            } else if cursor.kind == CXCursor_EnumDecl {
                let kind = if clang_getEnumDeclIntegerType(cursor).kind == CXType_Int {
                    IInt
                } else {
                    IUInt
                };
                let ei = mk_enuminfo(spelling, kind);
                GEnumDecl(ei)
            } else if cursor.kind == CXCursor_TypedefDecl {
                let ti = mk_typeinfo(spelling, @mut TVoid);
                GType(ti)
            } else if cursor.kind == CXCursor_VarDecl {
                let vi = mk_varinfo(spelling, @mut TVoid);
                GVar(vi)
            } else if cursor.kind == CXCursor_FunctionDecl {
                let vi = mk_varinfo(spelling, @mut TVoid);
                GFunc(vi)
            } else {
                GOther
            };

            ctx.name.insert(cursor, decl);
            return decl;
        }
    }
}

unsafe fn opaque_decl(ctx: @mut BindGenCtx, decl: CXCursor) {
    let name = decl_name(ctx, decl);
    ctx.globals.push(name);
}

unsafe fn fwd_decl(ctx: @mut BindGenCtx, cursor: CXCursor, f: &fn()) {
    let def = clang_getCursorDefinition(cursor);
    if cursor == def {
        f();
    } else if def.kind == CXCursor_NoDeclFound ||
              def.kind == CXCursor_InvalidFile {
        opaque_decl(ctx, cursor);
    }
}

unsafe fn conv_ptr_ty(ctx: @mut BindGenCtx, ty: CXType, cursor: CXCursor) -> @mut Type {
    if ty.kind == CXType_Void {
        return @mut TPtr(@mut TVoid)
    } else if ty.kind == CXType_Unexposed ||
              ty.kind == CXType_FunctionProto ||
              ty.kind == CXType_FunctionNoProto {
        let ret_ty = clang_getResultType(ty);
        let decl = clang_getTypeDeclaration(ty);
        return if ret_ty.kind != CXType_Invalid {
            let arg_n = clang_getNumArgTypes(ty) as uint;
            let args_lst = do vec::from_fn(arg_n) |i| {
                (~"", conv_ty(ctx, clang_getArgType(ty, i as c_uint), cursor))
            };

            let varargs = clang_isFunctionTypeVariadic(ty) as int != 0;
            let ret_ty = conv_ty(ctx, clang_getResultType(ty), cursor);

            @mut TFunc(ret_ty, args_lst, varargs)
        } else if decl.kind != CXCursor_NoDeclFound {
            @mut TPtr(conv_decl_ty(ctx, decl))
        } else {
            @mut TPtr(@mut TVoid)
        };
    } else if ty.kind == CXType_Typedef {
        let decl = clang_getTypeDeclaration(ty);
        let def_ty = clang_getTypedefDeclUnderlyingType(decl);
        if def_ty.kind == CXType_FunctionProto ||
           def_ty.kind == CXType_FunctionNoProto {
            return @mut TPtr(conv_ptr_ty(ctx, def_ty, cursor));
        }
    }
    return @mut TPtr(conv_ty(ctx, ty, cursor));
}

unsafe fn conv_decl_ty(ctx: @mut BindGenCtx, cursor: CXCursor) -> @mut Type {
    return if cursor.kind == CXCursor_StructDecl {
        let decl = decl_name(ctx, cursor);
        let ci = global_compinfo(decl);
        @mut TComp(ci)
    } else if cursor.kind == CXCursor_UnionDecl {
        let decl = decl_name(ctx, cursor);
        let ci = global_compinfo(decl);
        @mut TComp(ci)
    } else if cursor.kind == CXCursor_EnumDecl {
        let decl = decl_name(ctx, cursor);
        let ei = global_enuminfo(decl);
        @mut TEnum(ei)
    } else if cursor.kind == CXCursor_TypedefDecl {
        let decl = decl_name(ctx, cursor);
        let ti = global_typeinfo(decl);
        @mut TNamed(ti)
    } else {
        @mut TVoid
    };
}

unsafe fn conv_ty(ctx: @mut BindGenCtx, ty: CXType, cursor: CXCursor) -> @mut Type {
    return if ty.kind == CXType_Bool {
        @mut TInt(IBool)
    } else if ty.kind == CXType_SChar ||
              ty.kind == CXType_Char_S {
        @mut TInt(ISChar)
    } else if ty.kind == CXType_UChar ||
              ty.kind == CXType_Char_U {
        @mut TInt(IUChar)
    } else if ty.kind == CXType_UShort {
        @mut TInt(IUShort)
    } else if ty.kind == CXType_UInt {
        @mut TInt(IUInt)
    } else if ty.kind == CXType_ULong {
        @mut TInt(IULong)
    } else if ty.kind == CXType_ULongLong {
        @mut TInt(IULongLong)
    } else if ty.kind == CXType_Short {
        @mut TInt(IShort)
    } else if ty.kind == CXType_Int {
        @mut TInt(IInt)
    } else if ty.kind == CXType_Long {
        @mut TInt(ILong)
    } else if ty.kind == CXType_LongLong {
        @mut TInt(ILongLong)
    } else if ty.kind == CXType_Float {
        @mut TFloat(FFloat)
    } else if ty.kind == CXType_Double {
        @mut TFloat(FDouble)
    } else if ty.kind == CXType_LongDouble {
        @mut TFloat(FDouble)
    } else if ty.kind == CXType_Pointer {
        conv_ptr_ty(ctx, clang_getPointeeType(ty), cursor)
    } else if ty.kind == CXType_Record ||
              ty.kind == CXType_Typedef  ||
              ty.kind == CXType_Unexposed ||
              ty.kind == CXType_Enum {
        conv_decl_ty(ctx, clang_getTypeDeclaration(ty))
    } else if ty.kind == CXType_ConstantArray {
        let a_ty = conv_ty(ctx, clang_getArrayElementType(ty), cursor);
        let size = clang_getArraySize(ty) as uint;
        @mut TArray(a_ty, size)
    } else {
        @mut TVoid
    };
}

unsafe fn opaque_ty(ctx: @mut BindGenCtx, ty: CXType) {
    if ty.kind == CXType_Record || ty.kind == CXType_Enum {
        let decl = clang_getTypeDeclaration(ty);
        let def = clang_getCursorDefinition(decl);
        if def.kind == CXCursor_NoDeclFound ||
           def.kind == CXCursor_InvalidFile {
            opaque_decl(ctx, decl);
        }
    }
}

extern fn visit_struct(+cursor: CXCursor,
                       +_parent: CXCursor,
                       data: CXClientData) -> c_uint {
    unsafe {
        let ctx = *(data as *@mut BindGenCtx);
        if cursor.kind == CXCursor_FieldDecl {
            let ci = global_compinfo(ctx.cur_glob);
            let ty = conv_ty(ctx, clang_getCursorType(cursor), cursor);
            let name = clang_getCursorSpelling(cursor).to_str();
            let field = mk_fieldinfo(name, ty, ci);
            ci.fields.push(field);
        }
        return CXChildVisit_Continue;
    }
}

extern fn visit_union(+cursor: CXCursor,
                      +_parent: CXCursor,
                      data: CXClientData) -> c_uint {
    unsafe {
        let ctx = *(data as *@mut BindGenCtx);
        if cursor.kind == CXCursor_FieldDecl {
            let ci = global_compinfo(ctx.cur_glob);
            let ty = conv_ty(ctx, clang_getCursorType(cursor), cursor);
            let name = clang_getCursorSpelling(cursor).to_str();
            let field = mk_fieldinfo(name, ty, ci);
            ci.fields.push(field);
        }
        return CXChildVisit_Continue;
    }
}

extern fn visit_enum(+cursor: CXCursor,
                     +_parent: CXCursor,
                     data: CXClientData) -> c_uint {
    unsafe {
        let ctx = *(data as *@mut BindGenCtx);
        if cursor.kind == CXCursor_EnumConstantDecl {
            let ei = global_enuminfo(ctx.cur_glob);
            let name = clang_getCursorSpelling(cursor).to_str();
            let val = clang_getEnumConstantDeclValue(cursor) as int;
            let item = mk_enumitem(name, val, ei);
            ei.items.push(item);
        }
        return CXChildVisit_Continue;
    }
}

extern fn visit_top(+cursor: CXCursor,
                    +_parent: CXCursor,
                    data: CXClientData) -> c_uint {
    unsafe {
        let ctx = *(data as *@mut BindGenCtx);
        if !match_pattern(ctx, cursor) {
            return CXChildVisit_Continue;
        }
    
        if cursor.kind == CXCursor_StructDecl {
            do fwd_decl(ctx, cursor) || {
                let decl = decl_name(ctx, cursor);
                let ci = global_compinfo(decl);
                ctx.cur_glob = GComp(ci);
                ctx.globals.push(ctx.cur_glob);
                clang_visitChildren(cursor, visit_struct, data);
            }
            return CXChildVisit_Recurse;
        } else if cursor.kind == CXCursor_UnionDecl {
            do fwd_decl(ctx, cursor) || {
                let decl = decl_name(ctx, cursor);
                let ci = global_compinfo(decl);
                ctx.cur_glob = GComp(ci);
                ctx.globals.push(ctx.cur_glob);
                clang_visitChildren(cursor, visit_union, data);
            }
            return CXChildVisit_Recurse;
        } else if cursor.kind == CXCursor_EnumDecl {
            do fwd_decl(ctx, cursor) || {
                let decl = decl_name(ctx, cursor);
                let ei = global_enuminfo(decl);
                ctx.cur_glob = GEnum(ei);
                ctx.globals.push(ctx.cur_glob);
                clang_visitChildren(cursor, visit_enum, data);
            }
            return CXChildVisit_Continue;
        } else if cursor.kind == CXCursor_FunctionDecl {
            let linkage = clang_getCursorLinkage(cursor);
            if linkage != CXLinkage_External && linkage != CXLinkage_UniqueExternal {
                return CXChildVisit_Continue;
            }
    
            if ctx.name.contains_key(&cursor) {
                return CXChildVisit_Continue;
            }
    
            let arg_n = clang_Cursor_getNumArguments(cursor) as uint;
            let args_lst = do vec::from_fn(arg_n) |i| {
                let arg = clang_Cursor_getArgument(cursor, i as c_uint);
                let arg_name = clang_getCursorSpelling(arg).to_str();
                (arg_name, conv_ty(ctx, clang_getCursorType(arg), cursor))
            };
    
            let ty = clang_getCursorType(cursor);
            let varargs = clang_isFunctionTypeVariadic(ty) as int != 0;
            let ret_ty = conv_ty(ctx, clang_getCursorResultType(cursor), cursor);
    
            let func = decl_name(ctx, cursor);
            global_varinfo(func).ty = @mut TFunc(ret_ty, args_lst, varargs);
            ctx.globals.push(func);
    
            return CXChildVisit_Continue;
        } else if cursor.kind == CXCursor_VarDecl {
            let linkage = clang_getCursorLinkage(cursor);
            if linkage != CXLinkage_External && linkage != CXLinkage_UniqueExternal {
                return CXChildVisit_Continue;
            }
    
            if ctx.name.contains_key(&cursor) {
                return CXChildVisit_Continue;
            }
    
            let ty = conv_ty(ctx, clang_getCursorType(cursor), cursor);
            let var = decl_name(ctx, cursor);
            global_varinfo(var).ty = ty;
            ctx.globals.push(var);
    
            return CXChildVisit_Continue;
        } else if cursor.kind == CXCursor_TypedefDecl {
            if ctx.name.contains_key(&cursor) {
                return CXChildVisit_Continue;
            }
    
            let mut under_ty = clang_getTypedefDeclUnderlyingType(cursor);
            if under_ty.kind == CXType_Unexposed {
                under_ty = clang_getCanonicalType(under_ty);
            }
    
            let ty = conv_ty(ctx, under_ty, cursor);
            let typedef = decl_name(ctx, cursor);
            global_typeinfo(typedef).ty = ty;
            ctx.globals.push(typedef);
    
            opaque_ty(ctx, under_ty);
    
            return CXChildVisit_Continue;
        } else if cursor.kind == CXCursor_FieldDecl {
            return CXChildVisit_Continue;
        }
    
        return CXChildVisit_Recurse;
    }
}

fn main() {
    unsafe {
        let mut bind_args = os::args();
        let bin = bind_args.shift();
    
        match parse_args(bind_args) {
            ParseErr(e) => { fail!(e); }
            CmdUsage => { print_usage(bin); }
            ParseOk(clang_args, ctx) => {
                let ix = clang_createIndex(0 as c_int, 1 as c_int);
                if ix as int == 0 {
                    fail!(~"clang failed to create index");
                }
    
                let c_args = do clang_args.map |s| {
                    str::as_c_str(*s, |b| b )
                };
                let unit = clang_parseTranslationUnit(
                    ix, ptr::null(),
                    vec::raw::to_ptr(c_args),
                    vec::len(c_args) as c_int,
                    ptr::null(),
                    0 as c_uint, 0 as c_uint
                );
                if unit as int == 0 {
                    fail!(~"No input files given");
                }
    
                let mut c_err = false;
                let diag_num = clang_getNumDiagnostics(unit) as uint;
                for uint::range(0, diag_num) |i| {
                    let diag = clang_getDiagnostic(unit, i as c_uint);
                    io::stderr().write_line(clang_formatDiagnostic(
                        diag, clang_defaultDiagnosticDisplayOptions()
                    ).to_str());
    
                    if clang_getDiagnosticSeverity(diag) >= CXDiagnostic_Error {
                        c_err = true
                    }
                }
    
                if c_err {
                    return;
                }
    
                let cursor = clang_getTranslationUnitCursor(unit);
                clang_visitChildren(cursor, visit_top,
                                    ptr::to_unsafe_ptr(&ctx) as CXClientData);
                gen_rs(ctx.out, &ctx.link, ctx.globals);
    
                clang_disposeTranslationUnit(unit);
                clang_disposeIndex(ix);
            }
        }
    }
}
