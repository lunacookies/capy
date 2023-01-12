use std::{io::{self, Stdout, Write}, env, path::{self, PathBuf}};

use ast::AstNode;
use diagnostics::Severity;
use line_index::LineIndex;
use parser::parse_repl_line;
use rustc_hash::FxHashMap;
use std::fs;

fn main() -> io::Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    writeln!(stdout, "Capy Programming Language 0.1.0")?;

    let mut args = env::args();
    args.next(); // skip first arg as it's just the executable name
    if let Some(file_name) = args.next() {
        let file = fs::read_to_string(&file_name).expect("Unable to read file.");
        eval(&file_name.split(".").next().unwrap(), &file, &mut stdout)?;
        return Ok(());
    }

    let mut input = String::new();

    let mut continued = false;
    loop {
        write!(stdout, "{} ", if continued { " " } else { ">" })?;
        continued = false;
        stdout.flush()?;

        stdin.read_line(&mut input)?;
        if input.trim_end().ends_with("\\") {
            input = input.trim_end().to_string();
            input.pop();
            input.push('\n');
            continued = true;
            continue;
        }

        eval("repl", &input, &mut stdout)?;

        input.clear();
    }
}

fn eval(module: &str, input: &str, stdout: &mut Stdout) -> io::Result<()> {
    let mut interner = interner::Interner::default();

    let parse = parse_repl_line(&lexer::lex(&input), &input);
    writeln!(stdout, "{:?}", parse)?;

    let tree = parse.syntax_tree();

    let syntax_errors = parse
        .errors()
        .iter()
        .cloned()
        .map(diagnostics::Diagnostic::from_syntax);

    let root = ast::Root::cast(tree.root(), tree).unwrap();

    let validation_diagnostics = ast::validation::validate(root, tree);

    // let ast_vals = root
    //     .stmts(tree)
    //     .filter_map(|stmt| if let ast::Stmt::VarDef(var_def) = stmt {
    //         Some(var_def.value(tree))
    //     } else if let ast::Stmt::Return(ret) = stmt {
    //         Some(ret.value(tree))
    //     } else {
    //         None
    //     })
    //     .collect::<Vec<_>>();
    // dbg!(ast_vals);

    let mut world_index = hir::WorldIndex::default();

    let (index, indexing_diagnostics) = hir::index(root, tree, &mut interner);

    for name in index.definition_names() {
        println!(
            "{} = {:?}",
            interner.lookup(name.0),
            index.get_definition(name)
        )
    }

    let (bodies, lowering_diagnostics) =
        hir::lower(root, tree, &index, &world_index, &mut interner);

    println!("{}", bodies.debug(&interner));

    let (inference, type_diagnostics) = hir_types::infer_all(&bodies, &index, &world_index);

    println!("{}", inference.debug(&interner));

    // dbg!(hir::lower(root, tree));

    // hir_typed::infer_all

    let line_index = LineIndex::new(&input);

    let diagnostics: Vec<diagnostics::Diagnostic> = syntax_errors
        .chain(
            validation_diagnostics
                .iter()
                .cloned()
                .map(diagnostics::Diagnostic::from_validation),
        )
        .chain(
            indexing_diagnostics
                .iter()
                .cloned()
                .map(diagnostics::Diagnostic::from_indexing),
        )
        .chain(
            lowering_diagnostics
                .iter()
                .cloned()
                .map(diagnostics::Diagnostic::from_lowering),
        )
        .chain(
            type_diagnostics
                .iter()
                .cloned()
                .map(diagnostics::Diagnostic::from_type),
        )
        .collect();

    for diagnostic in &diagnostics {
        for line in diagnostic.display(&input, &interner, &line_index) {
            write!(stdout, "{}\n", line)?;
        }
    }
    if !diagnostics.is_empty() {
        write!(stdout, "\n")?;
    }

    if !diagnostics
        .iter()
        .any(|diag| diag.severity() == Severity::Error) {
        // compile to LLVM
        let module = hir::Name(interner.intern(module));
        world_index.add_module(module, index);

        let main = hir::Name(interner.intern("main"));

        let mut bodies_map = FxHashMap::default();
        let mut types_map = FxHashMap::default();
        bodies_map.insert(module, bodies);
        types_map.insert(module, inference);

        match codegen::compile(
            hir::Fqn {
                module,
                name: main,
            },
            &interner,
            bodies_map,
            types_map,
            &world_index,
        ) {
            Ok(output) => {
                let output_folder = env::current_dir().unwrap().join("out");
                let file = output_folder.join(&format!("{}.o", interner.lookup(module.0)));
                fs::write(&file, output.as_slice());
                println!(
                    "{}{}{}",
                    file.parent().and_then(|x| x.file_name()).and_then(|x| x.to_str()).unwrap(),
                    path::MAIN_SEPARATOR,
                    file.file_name().and_then(|x| x.to_str()).unwrap()
                );
                println!("Running\n");
                let exit_code = codegen::link_and_exec(&file);
                println!("\n\nProcess exited with code {}", exit_code);
            }
            Err(message) => {
                println!("error while finalizing: {}", message)
            }
        }
    } else {
        write!(stdout, "not compiling due to previous errors\n")?;
    }

    Ok(())
}
