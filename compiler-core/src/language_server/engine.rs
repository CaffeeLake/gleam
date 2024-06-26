use crate::{
    ast::{
        Arg, Definition, Import, ModuleConstant, Publicity, SrcSpan, TypedDefinition, TypedExpr,
        TypedFunction, TypedModule, TypedPattern,
    },
    build::{type_constructor_from_modules, Located, Module, UnqualifiedImport},
    config::PackageConfig,
    io::{CommandExecutor, FileSystemReader, FileSystemWriter},
    language_server::{
        compiler::LspProjectCompiler, files::FileSystemProxy, progress::ProgressReporter,
    },
    line_numbers::LineNumbers,
    paths::ProjectPaths,
    type_::{
        pretty::Printer, ModuleInterface, PreludeType, Type, TypeConstructor,
        ValueConstructorVariant,
    },
    Error, Result, Warning,
};
use camino::Utf8PathBuf;
use ecow::EcoString;
use lsp::CodeAction;
use lsp_types::{self as lsp, Hover, HoverContents, MarkedString, Url};
use std::sync::Arc;
use strum::IntoEnumIterator;

use super::{
    code_action::{CodeActionBuilder, RedundantTupleInCaseSubject},
    src_span_to_lsp_range, DownloadDependencies, MakeLocker,
};

#[derive(Debug, PartialEq, Eq)]
pub struct Response<T> {
    pub result: Result<T, Error>,
    pub warnings: Vec<Warning>,
    pub compilation: Compilation,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Compilation {
    /// Compilation was attempted and succeeded for these modules.
    Yes(Vec<Utf8PathBuf>),
    /// Compilation was not attempted for this operation.
    No,
}

#[derive(Debug)]
pub struct LanguageServerEngine<IO, Reporter> {
    pub(crate) paths: ProjectPaths,

    /// A compiler for the project that supports repeat compilation of the root
    /// package.
    /// In the event the project config changes this will need to be
    /// discarded and reloaded to handle any changes to dependencies.
    pub(crate) compiler: LspProjectCompiler<FileSystemProxy<IO>>,

    modules_compiled_since_last_feedback: Vec<Utf8PathBuf>,
    compiled_since_last_feedback: bool,

    // Used to publish progress notifications to the client without waiting for
    // the usual request-response loop.
    progress_reporter: Reporter,

    /// Used to know if to show the "View on HexDocs" link
    /// when hovering on an imported value
    hex_deps: std::collections::HashSet<EcoString>,
}

impl<'a, IO, Reporter> LanguageServerEngine<IO, Reporter>
where
    // IO to be supplied from outside of gleam-core
    IO: FileSystemReader
        + FileSystemWriter
        + CommandExecutor
        + DownloadDependencies
        + MakeLocker
        + Clone,
    // IO to be supplied from inside of gleam-core
    Reporter: ProgressReporter + Clone + 'a,
{
    pub fn new(
        config: PackageConfig,
        progress_reporter: Reporter,
        io: FileSystemProxy<IO>,
        paths: ProjectPaths,
    ) -> Result<Self> {
        let locker = io.inner().make_locker(&paths, config.target)?;

        // Download dependencies to ensure they are up-to-date for this new
        // configuration and new instance of the compiler
        progress_reporter.dependency_downloading_started();
        let manifest = io.inner().download_dependencies(&paths);
        progress_reporter.dependency_downloading_finished();

        // NOTE: This must come after the progress reporter has finished!
        let manifest = manifest?;

        let compiler =
            LspProjectCompiler::new(manifest, config, paths.clone(), io.clone(), locker)?;

        let hex_deps = compiler
            .project_compiler
            .packages
            .iter()
            .flat_map(|(k, v)| match &v.source {
                crate::manifest::ManifestPackageSource::Hex { .. } => {
                    Some(EcoString::from(k.as_str()))
                }

                _ => None,
            })
            .collect();

        Ok(Self {
            modules_compiled_since_last_feedback: vec![],
            compiled_since_last_feedback: false,
            progress_reporter,
            compiler,
            paths,
            hex_deps,
        })
    }

    pub fn compile_please(&mut self) -> Response<()> {
        self.respond(Self::compile)
    }

    /// Compile the project if we are in one. Otherwise do nothing.
    fn compile(&mut self) -> Result<(), Error> {
        self.compiled_since_last_feedback = true;

        self.progress_reporter.compilation_started();
        let result = self.compiler.compile();
        self.progress_reporter.compilation_finished();

        let modules = result?;
        self.modules_compiled_since_last_feedback.extend(modules);

        Ok(())
    }

    fn take_warnings(&mut self) -> Vec<Warning> {
        self.compiler.take_warnings()
    }

    // TODO: test different package module function calls
    //
    // TODO: implement unqualified imported module functions
    //
    pub fn goto_definition(
        &mut self,
        params: lsp::GotoDefinitionParams,
    ) -> Response<Option<lsp::Location>> {
        self.respond(|this| {
            let params = params.text_document_position_params;
            let (line_numbers, node) = match this.node_at_position(&params) {
                Some(location) => location,
                None => return Ok(None),
            };

            let location = match node
                .definition_location(this.compiler.project_compiler.get_importable_modules())
            {
                Some(location) => location,
                None => return Ok(None),
            };

            let (uri, line_numbers) = match location.module {
                None => (params.text_document.uri, &line_numbers),
                Some(name) => {
                    let module = match this.compiler.get_source(name) {
                        Some(module) => module,
                        _ => return Ok(None),
                    };
                    let url = Url::parse(&format!("file:///{}", &module.path))
                        .expect("goto definition URL parse");
                    (url, &module.line_numbers)
                }
            };
            let range = src_span_to_lsp_range(location.span, line_numbers);

            Ok(Some(lsp::Location { uri, range }))
        })
    }

    pub fn completion(
        &mut self,
        params: lsp::TextDocumentPositionParams,
        src: EcoString,
    ) -> Response<Option<Vec<lsp::CompletionItem>>> {
        self.respond(|this| {
            let module = match this.module_for_uri(&params.text_document.uri) {
                Some(m) => m,
                None => return Ok(None),
            };

            // Check current filercontents if the user is writing an import
            // and handle separately from the rest of the completion flow
            // Check if an import is being written
            if let Some(value) = this.import_completions(src, &params, module) {
                return value;
            }

            let line_numbers = LineNumbers::new(&module.code);
            let byte_index =
                line_numbers.byte_index(params.position.line, params.position.character);

            let Some(found) = module.find_node(byte_index) else {
                return Ok(None);
            };

            let completions = match found {
                Located::Pattern(_pattern) => None,

                Located::Statement(_) | Located::Expression(_) => {
                    Some(this.completion_values(module))
                }

                Located::ModuleStatement(Definition::Function(_)) => {
                    Some(this.completion_types(module))
                }

                Located::FunctionBody(_) => Some(this.completion_values(module)),

                Located::ModuleStatement(Definition::TypeAlias(_) | Definition::CustomType(_)) => {
                    Some(this.completion_types(module))
                }

                // If the import completions returned no results and we are in an import then
                // we should try to provide completions for unqualified values
                Located::ModuleStatement(Definition::Import(import)) => this
                    .compiler
                    .get_module_inferface(import.module.as_str())
                    .map(|importing_module| {
                        this.unqualified_completions_from_module(importing_module, module, true)
                    }),

                Located::ModuleStatement(Definition::ModuleConstant(_)) => None,

                Located::UnqualifiedImport(_) => None,

                Located::Arg(_) => None,

                Located::Annotation(_, _) => Some(this.completion_types(module)),
            };

            Ok(completions)
        })
    }

    pub fn action(&mut self, params: lsp::CodeActionParams) -> Response<Option<Vec<CodeAction>>> {
        self.respond(|this| {
            let mut actions = vec![];
            let Some(module) = this.module_for_uri(&params.text_document.uri) else {
                return Ok(None);
            };

            code_action_unused_imports(module, &params, &mut actions);
            actions.extend(RedundantTupleInCaseSubject::new(module, &params).code_actions());

            Ok(if actions.is_empty() {
                None
            } else {
                Some(actions)
            })
        })
    }

    fn respond<T>(&mut self, handler: impl FnOnce(&mut Self) -> Result<T>) -> Response<T> {
        let result = handler(self);
        let warnings = self.take_warnings();
        // TODO: test. Ensure hover doesn't report as compiled
        let compilation = if self.compiled_since_last_feedback {
            let modules = std::mem::take(&mut self.modules_compiled_since_last_feedback);
            self.compiled_since_last_feedback = false;
            Compilation::Yes(modules)
        } else {
            Compilation::No
        };
        Response {
            result,
            warnings,
            compilation,
        }
    }

    pub fn hover(&mut self, params: lsp::HoverParams) -> Response<Option<Hover>> {
        self.respond(|this| {
            let params = params.text_document_position_params;

            let (lines, found) = match this.node_at_position(&params) {
                Some(value) => value,
                None => return Ok(None),
            };

            Ok(match found {
                Located::Statement(_) => None, // TODO: hover for statement
                Located::ModuleStatement(Definition::Function(fun)) => {
                    Some(hover_for_function_head(fun, lines))
                }
                Located::ModuleStatement(Definition::ModuleConstant(constant)) => {
                    Some(hover_for_module_constant(constant, lines))
                }
                Located::ModuleStatement(_) => None,
                Located::UnqualifiedImport(UnqualifiedImport {
                    name,
                    module,
                    is_type,
                    location,
                }) => this
                    .compiler
                    .get_module_inferface(module.as_str())
                    .and_then(|module| {
                        if is_type {
                            module.types.get(name).map(|t| {
                                hover_for_annotation(*location, t.typ.as_ref(), Some(t), lines)
                            })
                        } else {
                            module.values.get(name).map(|v| {
                                let m = if this.hex_deps.contains(&module.package) {
                                    Some(module)
                                } else {
                                    None
                                };
                                hover_for_imported_value(v, location, lines, m, name)
                            })
                        }
                    }),
                Located::Pattern(pattern) => Some(hover_for_pattern(pattern, lines)),
                Located::Expression(expression) => {
                    let module = this.module_for_uri(&params.text_document.uri);

                    Some(hover_for_expression(
                        expression,
                        lines,
                        module,
                        &this.hex_deps,
                    ))
                }
                Located::Arg(arg) => Some(hover_for_function_argument(arg, lines)),
                Located::FunctionBody(_) => None,
                Located::Annotation(annotation, type_) => {
                    let type_constructor = type_constructor_from_modules(
                        this.compiler.project_compiler.get_importable_modules(),
                        type_.clone(),
                    );
                    Some(hover_for_annotation(
                        annotation,
                        &type_,
                        type_constructor,
                        lines,
                    ))
                }
            })
        })
    }

    fn module_node_at_position(
        &self,
        params: &lsp::TextDocumentPositionParams,
        module: &'a Module,
    ) -> Option<(LineNumbers, Located<'a>)> {
        let line_numbers = LineNumbers::new(&module.code);
        let byte_index = line_numbers.byte_index(params.position.line, params.position.character);
        let node = module.find_node(byte_index);
        let node = node?;
        Some((line_numbers, node))
    }

    fn node_at_position(
        &self,
        params: &lsp::TextDocumentPositionParams,
    ) -> Option<(LineNumbers, Located<'_>)> {
        let module = self.module_for_uri(&params.text_document.uri)?;
        self.module_node_at_position(params, module)
    }

    fn module_for_uri(&self, uri: &Url) -> Option<&Module> {
        use itertools::Itertools;

        // The to_file_path method is available on these platforms
        #[cfg(any(unix, windows, target_os = "redox", target_os = "wasi"))]
        let path = uri.to_file_path().expect("URL file");

        #[cfg(not(any(unix, windows, target_os = "redox", target_os = "wasi")))]
        let path: Utf8PathBuf = uri.path().into();

        let components = path
            .strip_prefix(self.paths.root())
            .ok()?
            .components()
            .skip(1)
            .map(|c| c.as_os_str().to_string_lossy());
        let module_name: EcoString = Itertools::intersperse(components, "/".into())
            .collect::<String>()
            .strip_suffix(".gleam")?
            .into();

        self.compiler.modules.get(&module_name)
    }

    /// checks based on the publicity if something should be suggested for import from root package
    fn is_suggestable_import(&self, publicity: &Publicity, package: &str) -> bool {
        match publicity {
            // We skip private types as we never want those to appear in
            // completions.
            Publicity::Private => false,
            // We only skip internal types if those are not defined in
            // the root package.
            Publicity::Internal if package != self.root_package_name() => false,
            Publicity::Internal => true,
            // We never skip public types.
            Publicity::Public => true,
        }
    }

    fn completion_types<'b>(&'b self, module: &'b Module) -> Vec<lsp::CompletionItem> {
        let mut completions = vec![];

        // Prelude types
        for type_ in PreludeType::iter() {
            completions.push(lsp::CompletionItem {
                label: type_.name().into(),
                detail: Some("Type".into()),
                kind: Some(lsp::CompletionItemKind::CLASS),
                ..Default::default()
            });
        }

        // Module types
        for (name, type_) in &module.ast.type_info.types {
            completions.push(type_completion(None, name, type_));
        }

        // Imported modules
        for import in module.ast.definitions.iter().filter_map(get_import) {
            // The module may not be known of yet if it has not previously
            // compiled yet in this editor session.
            // TODO: test getting completions from modules defined in other packages
            let Some(module) = self.compiler.get_module_inferface(&import.module) else {
                continue;
            };

            // Qualified types
            for (name, type_) in &module.types {
                if !self.is_suggestable_import(&type_.publicity, module.package.as_str()) {
                    continue;
                }

                let module = import.used_name();
                if module.is_some() {
                    completions.push(type_completion(module.as_ref(), name, type_));
                }
            }

            // Unqualified types
            for unqualified in &import.unqualified_types {
                match module.get_public_type(&unqualified.name) {
                    Some(type_) => {
                        completions.push(type_completion(None, unqualified.used_name(), type_))
                    }
                    None => continue,
                }
            }
        }

        completions
    }

    fn completion_values<'b>(&'b self, module: &'b Module) -> Vec<lsp::CompletionItem> {
        let mut completions = vec![];

        // Module functions
        for (name, value) in &module.ast.type_info.values {
            // Here we do not check for the internal attribute: we always want
            // to show autocompletions for values defined in the same module,
            // even if those are internal.
            completions.push(value_completion(None, name, value));
        }

        // Imported modules
        for import in module.ast.definitions.iter().filter_map(get_import) {
            // The module may not be known of yet if it has not previously
            // compiled yet in this editor session.
            // TODO: test getting completions from modules defined in other packages
            let Some(module) = self.compiler.get_module_inferface(&import.module) else {
                continue;
            };

            // Qualified values
            for (name, value) in &module.values {
                if !self.is_suggestable_import(&value.publicity, module.package.as_str()) {
                    continue;
                }

                let module = import.used_name();
                if module.is_some() {
                    completions.push(value_completion(module.as_deref(), name, value));
                }
            }

            // Unqualified values
            for unqualified in &import.unqualified_values {
                match module.get_public_value(&unqualified.name) {
                    Some(value) => {
                        completions.push(value_completion(None, unqualified.used_name(), value))
                    }
                    None => continue,
                }
            }
        }

        completions
    }

    fn unqualified_completions_from_module<'b>(
        &'b self,
        importing_module: &'b ModuleInterface,
        module: &'b Module,
        // should type completions include the word "type" in the completion
        include_type_in_completion: bool,
    ) -> Vec<lsp::CompletionItem> {
        let mut completions = vec![];

        // Find values and type that have already previously been imported
        let mut already_imported_types = std::collections::HashSet::new();
        let mut already_imported_values = std::collections::HashSet::new();

        // Search the ast for import statements
        for import in module.ast.definitions.iter().filter_map(get_import) {
            // Find the import that matches the module being imported from
            if import.module == importing_module.name {
                // Add the values and types that have already been imported
                for unqualified in &import.unqualified_types {
                    let _ = already_imported_types.insert(&unqualified.name);
                }

                for unqualified in &import.unqualified_values {
                    let _ = already_imported_values.insert(&unqualified.name);
                }
            }
        }

        // Get completable types
        for (name, type_) in &importing_module.types {
            // Skip types that should not be suggested
            if !self.is_suggestable_import(&type_.publicity, importing_module.package.as_str()) {
                continue;
            }

            // Skip type that are already imported
            if already_imported_types.contains(name) {
                continue;
            }

            let completion: lsp::CompletionItem = if !include_type_in_completion {
                type_completion(None, name, type_)
            } else {
                let completion = type_completion(None, name, type_);
                lsp::CompletionItem {
                    // Add type prior to unqualified import for types
                    insert_text: Some("type ".to_string() + &completion.label),
                    ..completion
                }
            };
            completions.push(completion);
        }

        // Get completable values
        for (name, value) in &importing_module.values {
            // Skip values that should not be suggested
            if !self.is_suggestable_import(&value.publicity, importing_module.package.as_str()) {
                continue;
            }

            // Skip values that are already imported
            if already_imported_values.contains(name) {
                continue;
            }
            completions.push(value_completion(None, name, value));
        }

        completions
    }

    fn import_completions<'b>(
        &'b self,
        src: EcoString,
        params: &lsp::TextDocumentPositionParams,
        module: &'b Module,
    ) -> Option<Result<Option<Vec<lsp::CompletionItem>>>> {
        let line_num = LineNumbers::new(src.as_str());
        let start_of_line = line_num.byte_index(params.position.line, 0);
        let end_of_line = line_num.byte_index(params.position.line + 1, 0);

        // Drop all lines except the line the cursor is on
        let src = &src.get(start_of_line as usize..end_of_line as usize)?;

        // If this isn't an import line then we don't offer import completions
        if !src.trim_start().starts_with("import") {
            return None;
        }

        // Check if we are completing an unqualified import
        if let Some(dot_index) = src.find('.') {
            // Find the module that is being imported from
            let importing_module_name = src.get(6..dot_index)?.trim();
            let importing_module: &ModuleInterface =
                self.compiler.get_module_inferface(importing_module_name)?;

            // Check if the cursor is proceeded by the word "type".
            // We want to make sure suggestions don't include the word "type"
            // if the cursor is proceeded by it.
            let cursor = src.get(..params.position.character as usize)?;
            Some(Ok(Some(self.unqualified_completions_from_module(
                importing_module,
                module,
                !cursor.trim().ends_with("type"),
            ))))
        } else {
            // Find where to start and end the import completion
            let start = line_num.line_and_column_number(start_of_line);
            let end = line_num.line_and_column_number(end_of_line - 1);
            let start = lsp::Position::new(start.line - 1, start.column + 6);
            let end = lsp::Position::new(end.line - 1, end.column - 1);
            let completions = self.complete_modules_for_import(module, start, end);

            Some(Ok(Some(completions)))
        }
    }

    fn complete_modules_for_import<'b>(
        &'b self,
        current_module: &'b Module,
        start: lsp::Position,
        end: lsp::Position,
    ) -> Vec<lsp::CompletionItem> {
        let mut direct_dep_packages: std::collections::HashSet<&EcoString> =
            std::collections::HashSet::from_iter(
                self.compiler.project_compiler.config.dependencies.keys(),
            );
        if !current_module.origin.is_src() {
            // In tests we can import direct dev dependencies
            direct_dep_packages.extend(
                self.compiler
                    .project_compiler
                    .config
                    .dev_dependencies
                    .keys(),
            )
        }

        let already_imported: std::collections::HashSet<EcoString> =
            std::collections::HashSet::from_iter(current_module.dependencies_list());
        self.compiler
            .project_compiler
            .get_importable_modules()
            .iter()
            //
            // It is possible to import modules from dependencies of dependencies
            // but it's not recommended so we don't include them in completions
            .filter(|(_, module)| {
                let is_root_or_prelude =
                    module.package == self.root_package_name() || module.package.is_empty();
                is_root_or_prelude || direct_dep_packages.contains(&module.package)
            })
            //
            // src/ cannot import test/
            .filter(|(_, module)| module.origin.is_src() || !current_module.origin.is_src())
            //
            // It is possible to import internal modules from other packages,
            // but it's not recommended so we don't include them in completions
            .filter(|(_, module)| module.package == self.root_package_name() || !module.is_internal)
            //
            // You cannot import a module twice
            .filter(|(name, _)| !already_imported.contains(*name))
            //
            // You cannot import yourself
            .filter(|(name, _)| *name != &current_module.name)
            //
            // Everything else we suggest as a completion
            .map(|(name, _)| lsp::CompletionItem {
                label: name.to_string(),
                kind: Some(lsp::CompletionItemKind::MODULE),
                text_edit: Some(lsp::CompletionTextEdit::Edit(lsp::TextEdit {
                    range: lsp::Range { start, end },
                    new_text: name.to_string(),
                })),
                ..Default::default()
            })
            .collect()
    }

    fn root_package_name(&self) -> &str {
        self.compiler.project_compiler.config.name.as_str()
    }
}

fn type_completion(
    module: Option<&EcoString>,
    name: &str,
    type_: &TypeConstructor,
) -> lsp::CompletionItem {
    let label = match module {
        Some(module) => format!("{module}.{name}"),
        None => name.to_string(),
    };

    let kind = Some(if type_.typ.is_variable() {
        lsp::CompletionItemKind::VARIABLE
    } else {
        lsp::CompletionItemKind::CLASS
    });

    lsp::CompletionItem {
        label,
        kind,
        detail: Some("Type".into()),
        ..Default::default()
    }
}

fn value_completion(
    module: Option<&str>,
    name: &str,
    value: &crate::type_::ValueConstructor,
) -> lsp::CompletionItem {
    let label = match module {
        Some(module) => format!("{module}.{name}"),
        None => name.to_string(),
    };

    let type_ = Printer::new().pretty_print(&value.type_, 0);

    let kind = Some(match value.variant {
        ValueConstructorVariant::LocalVariable { .. } => lsp::CompletionItemKind::VARIABLE,
        ValueConstructorVariant::ModuleConstant { .. } => lsp::CompletionItemKind::CONSTANT,
        ValueConstructorVariant::LocalConstant { .. } => lsp::CompletionItemKind::CONSTANT,
        ValueConstructorVariant::ModuleFn { .. } => lsp::CompletionItemKind::FUNCTION,
        ValueConstructorVariant::Record { arity: 0, .. } => lsp::CompletionItemKind::ENUM_MEMBER,
        ValueConstructorVariant::Record { .. } => lsp::CompletionItemKind::CONSTRUCTOR,
    });

    let documentation = value.get_documentation().map(|d| {
        lsp::Documentation::MarkupContent(lsp::MarkupContent {
            kind: lsp::MarkupKind::Markdown,
            value: d.to_string(),
        })
    });

    lsp::CompletionItem {
        label,
        kind,
        detail: Some(type_),
        documentation,
        ..Default::default()
    }
}

fn get_import(statement: &TypedDefinition) -> Option<&Import<EcoString>> {
    match statement {
        Definition::Import(import) => Some(import),
        _ => None,
    }
}

fn hover_for_pattern(pattern: &TypedPattern, line_numbers: LineNumbers) -> Hover {
    let documentation = pattern.get_documentation().unwrap_or_default();

    // Show the type of the hovered node to the user
    let type_ = Printer::new().pretty_print(pattern.type_().as_ref(), 0);
    let contents = format!(
        "```gleam
{type_}
```
{documentation}"
    );
    Hover {
        contents: HoverContents::Scalar(MarkedString::String(contents)),
        range: Some(src_span_to_lsp_range(pattern.location(), &line_numbers)),
    }
}

fn hover_for_function_head(fun: &TypedFunction, line_numbers: LineNumbers) -> Hover {
    let empty_str = EcoString::from("");
    let documentation = fun.documentation.as_ref().unwrap_or(&empty_str);
    let function_type = Type::Fn {
        args: fun.arguments.iter().map(|arg| arg.type_.clone()).collect(),
        retrn: fun.return_type.clone(),
    };
    let formatted_type = Printer::new().pretty_print(&function_type, 0);
    let contents = format!(
        "```gleam
{formatted_type}
```
{documentation}"
    );
    Hover {
        contents: HoverContents::Scalar(MarkedString::String(contents)),
        range: Some(src_span_to_lsp_range(fun.location, &line_numbers)),
    }
}

fn hover_for_function_argument(argument: &Arg<Arc<Type>>, line_numbers: LineNumbers) -> Hover {
    let type_ = Printer::new().pretty_print(&argument.type_, 0);
    let contents = format!("```gleam\n{type_}\n```");
    Hover {
        contents: HoverContents::Scalar(MarkedString::String(contents)),
        range: Some(src_span_to_lsp_range(argument.location, &line_numbers)),
    }
}

fn hover_for_annotation(
    location: SrcSpan,
    annotation_type: &Type,
    type_constructor: Option<&TypeConstructor>,
    line_numbers: LineNumbers,
) -> Hover {
    let empty_str = EcoString::from("");
    let documentation = type_constructor
        .and_then(|t| t.documentation.as_ref())
        .unwrap_or(&empty_str);
    let type_ = Printer::new().pretty_print(annotation_type, 0);
    let contents = format!(
        "```gleam
{type_}
```
{documentation}"
    );
    Hover {
        contents: HoverContents::Scalar(MarkedString::String(contents)),
        range: Some(src_span_to_lsp_range(location, &line_numbers)),
    }
}

fn hover_for_module_constant(
    constant: &ModuleConstant<Arc<Type>, EcoString>,
    line_numbers: LineNumbers,
) -> Hover {
    let empty_str = EcoString::from("");
    let type_ = Printer::new().pretty_print(&constant.type_, 0);
    let documentation = constant.documentation.as_ref().unwrap_or(&empty_str);
    let contents = format!("```gleam\n{type_}\n```\n{documentation}");
    Hover {
        contents: HoverContents::Scalar(MarkedString::String(contents)),
        range: Some(src_span_to_lsp_range(constant.location, &line_numbers)),
    }
}

fn hover_for_expression(
    expression: &TypedExpr,
    line_numbers: LineNumbers,
    module: Option<&Module>,
    hex_deps: &std::collections::HashSet<EcoString>,
) -> Hover {
    let documentation = expression.get_documentation().unwrap_or_default();

    let link_section = module
        .and_then(|m: &Module| {
            let (module_name, name) = get_expr_qualified_name(expression)?;
            get_hexdocs_link_section(module_name, name, &m.ast, hex_deps)
        })
        .unwrap_or("".to_string());

    // Show the type of the hovered node to the user
    let type_ = Printer::new().pretty_print(expression.type_().as_ref(), 0);
    let contents = format!(
        "```gleam
{type_}
```
{documentation}{link_section}"
    );
    Hover {
        contents: HoverContents::Scalar(MarkedString::String(contents)),
        range: Some(src_span_to_lsp_range(expression.location(), &line_numbers)),
    }
}

fn hover_for_imported_value(
    value: &crate::type_::ValueConstructor,
    location: &SrcSpan,
    line_numbers: LineNumbers,
    hex_module_imported_from: Option<&ModuleInterface>,
    name: &EcoString,
) -> Hover {
    let documentation = value.get_documentation().unwrap_or_default();

    let link_section = hex_module_imported_from.map_or("".to_string(), |m| {
        format_hexdocs_link_section(m.package.as_str(), m.name.as_str(), name)
    });

    // Show the type of the hovered node to the user
    let type_ = Printer::new().pretty_print(value.type_.as_ref(), 0);
    let contents = format!(
        "```gleam
{type_}
```
{documentation}{link_section}"
    );
    Hover {
        contents: HoverContents::Scalar(MarkedString::String(contents)),
        range: Some(src_span_to_lsp_range(*location, &line_numbers)),
    }
}

// Returns true if any part of either range overlaps with the other.
pub fn overlaps(a: lsp_types::Range, b: lsp_types::Range) -> bool {
    within(a.start, b) || within(a.end, b) || within(b.start, a) || within(b.end, a)
}

// Returns true if a position is within a range
fn within(position: lsp_types::Position, range: lsp_types::Range) -> bool {
    position >= range.start && position < range.end
}

fn code_action_unused_imports(
    module: &Module,
    params: &lsp::CodeActionParams,
    actions: &mut Vec<CodeAction>,
) {
    let uri = &params.text_document.uri;
    let unused = &module.ast.type_info.unused_imports;

    if unused.is_empty() {
        return;
    }

    // Convert src spans to lsp range
    let line_numbers = LineNumbers::new(&module.code);
    let mut hovered = false;
    let mut edits = Vec::with_capacity(unused.len());

    for unused in unused {
        let SrcSpan { start, end } = *unused;

        // If removing an unused alias or at the beginning of the file, don't backspace
        // Otherwise, adjust the end position by 1 to ensure the entire line is deleted with the import.
        let adjusted_end = if delete_line(unused, &line_numbers) {
            end + 1
        } else {
            end
        };

        let range = src_span_to_lsp_range(SrcSpan::new(start, adjusted_end), &line_numbers);
        // Keep track of whether any unused import has is where the cursor is
        hovered = hovered || overlaps(params.range, range);

        edits.push(lsp_types::TextEdit {
            range,
            new_text: "".into(),
        });
    }

    // If none of the imports are where the cursor is we do nothing
    if !hovered {
        return;
    }
    edits.sort_by_key(|edit| edit.range.start);

    CodeActionBuilder::new("Remove unused imports")
        .kind(lsp_types::CodeActionKind::QUICKFIX)
        .changes(uri.clone(), edits)
        .preferred(true)
        .push_to(actions);
}

// Check if the edit empties a whole line; if so, delete the line.
fn delete_line(span: &SrcSpan, line_numbers: &LineNumbers) -> bool {
    line_numbers.line_starts.iter().any(|&line_start| {
        line_start == span.start && line_numbers.line_starts.contains(&(span.end + 1))
    })
}

fn get_expr_qualified_name(expression: &TypedExpr) -> Option<(&EcoString, &EcoString)> {
    match expression {
        TypedExpr::Var {
            name, constructor, ..
        } if constructor.publicity.is_importable() => match &constructor.variant {
            ValueConstructorVariant::ModuleFn {
                module: module_name,
                ..
            } => Some((module_name, name)),

            ValueConstructorVariant::ModuleConstant {
                module: module_name,
                ..
            } => Some((module_name, name)),

            _ => None,
        },

        TypedExpr::ModuleSelect {
            label, module_name, ..
        } => Some((module_name, label)),

        _ => None,
    }
}

fn format_hexdocs_link_section(package_name: &str, module_name: &str, name: &str) -> String {
    let link = format!("https://hexdocs.pm/{package_name}/{module_name}.html#{name}");
    format!("\nView on [HexDocs]({link})")
}

fn get_hexdocs_link_section(
    module_name: &str,
    name: &str,
    ast: &TypedModule,
    hex_deps: &std::collections::HashSet<EcoString>,
) -> Option<String> {
    let package_name = ast.definitions.iter().find_map(|def| match def {
        Definition::Import(p) if p.module == module_name && hex_deps.contains(&p.package) => {
            Some(&p.package)
        }
        _ => None,
    })?;

    Some(format_hexdocs_link_section(package_name, module_name, name))
}
