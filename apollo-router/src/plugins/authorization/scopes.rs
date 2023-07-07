//! Authorization plugin

use std::collections::HashSet;

use apollo_compiler::hir;
use apollo_compiler::hir::FieldDefinition;
use apollo_compiler::hir::TypeDefinition;
use apollo_compiler::hir::Value;
use apollo_compiler::ApolloCompiler;
use apollo_compiler::FileId;
use apollo_compiler::HirDatabase;
use tower::BoxError;

use crate::json_ext::Path;
use crate::json_ext::PathElement;
use crate::spec::query::transform;
use crate::spec::query::traverse;

pub(crate) struct ScopeExtractionVisitor<'a> {
    compiler: &'a ApolloCompiler,
    pub(crate) extracted_scopes: HashSet<String>,
}

pub(crate) const REQUIRES_SCOPES_DIRECTIVE_NAME: &str = "requiresScopes";

impl<'a> ScopeExtractionVisitor<'a> {
    #[allow(dead_code)]
    pub(crate) fn new(compiler: &'a ApolloCompiler) -> Self {
        Self {
            compiler,
            extracted_scopes: HashSet::new(),
        }
    }

    fn get_scopes_from_field(&mut self, field: &FieldDefinition) {
        self.extracted_scopes.extend(
            scopes_argument(field.directive_by_name(REQUIRES_SCOPES_DIRECTIVE_NAME)).cloned(),
        );

        if let Some(ty) = field.ty().type_def(&self.compiler.db) {
            self.extracted_scopes.extend(
                scopes_argument(ty.directive_by_name(REQUIRES_SCOPES_DIRECTIVE_NAME)).cloned(),
            );
        }
    }
}

fn scopes_argument(opt_directive: Option<&hir::Directive>) -> impl Iterator<Item = &String> {
    opt_directive
        .and_then(|directive| directive.argument_by_name("scopes"))
        .and_then(|value| match value {
            Value::List { value, .. } => Some(value),
            _ => None,
        })
        .into_iter()
        .flatten()
        .filter_map(|v| match v {
            Value::String { value, .. } => Some(value),
            _ => None,
        })
}

impl<'a> traverse::Visitor for ScopeExtractionVisitor<'a> {
    fn compiler(&self) -> &ApolloCompiler {
        self.compiler
    }

    fn field(&mut self, parent_type: &str, node: &hir::Field) -> Result<(), BoxError> {
        if let Some(ty) = self
            .compiler
            .db
            .types_definitions_by_name()
            .get(parent_type)
        {
            if let Some(field) = ty.field(&self.compiler.db, node.name()) {
                self.get_scopes_from_field(field);
            }
        }

        traverse::field(self, parent_type, node)
    }
}

pub(crate) struct ScopeFilteringVisitor<'a> {
    compiler: &'a ApolloCompiler,
    file_id: FileId,
    request_scopes: HashSet<String>,
    pub(crate) query_requires_scopes: bool,
    pub(crate) unauthorized_paths: Vec<Path>,
    current_path: Path,
}

impl<'a> ScopeFilteringVisitor<'a> {
    pub(crate) fn new(
        compiler: &'a ApolloCompiler,
        file_id: FileId,
        scopes: HashSet<String>,
    ) -> Self {
        Self {
            compiler,
            file_id,
            request_scopes: scopes,
            query_requires_scopes: false,
            unauthorized_paths: vec![],
            current_path: Path::default(),
        }
    }

    fn is_field_authorized(&mut self, field: &FieldDefinition) -> bool {
        let field_scopes = scopes_argument(field.directive_by_name(REQUIRES_SCOPES_DIRECTIVE_NAME))
            .cloned()
            .collect::<HashSet<_>>();

        if !self.request_scopes.is_superset(&field_scopes) {
            return false;
        }

        if let Some(ty) = field.ty().type_def(&self.compiler.db) {
            self.is_type_authorized(&ty)
        } else {
            false
        }
    }

    fn is_type_authorized(&self, ty: &TypeDefinition) -> bool {
        let type_scopes = scopes_argument(ty.directive_by_name(REQUIRES_SCOPES_DIRECTIVE_NAME))
            .cloned()
            .collect::<HashSet<_>>();

        self.request_scopes.is_superset(&type_scopes)
    }
}

impl<'a> transform::Visitor for ScopeFilteringVisitor<'a> {
    fn compiler(&self) -> &ApolloCompiler {
        self.compiler
    }

    fn field(
        &mut self,
        parent_type: &str,
        node: &hir::Field,
    ) -> Result<Option<apollo_encoder::Field>, BoxError> {
        let field_name = node.name();
        let mut is_field_list = false;

        let is_authorized = self
            .compiler
            .db
            .types_definitions_by_name()
            .get(parent_type)
            .is_some_and(|def| {
                if let Some(field) = def.field(&self.compiler.db, field_name) {
                    if field.ty().is_list() {
                        is_field_list = true;
                    }
                    self.is_field_authorized(field)
                } else {
                    false
                }
            });

        self.current_path.push(PathElement::Key(field_name.into()));
        if is_field_list {
            self.current_path.push(PathElement::Flatten);
        }

        if !is_authorized {
            self.unauthorized_paths.push(self.current_path.clone());
        }

        let res = if is_authorized {
            transform::field(self, parent_type, node)
        } else {
            self.query_requires_scopes = true;
            Ok(None)
        };

        if is_field_list {
            self.current_path.pop();
        }
        self.current_path.pop();

        res
    }

    fn fragment_definition(
        &mut self,
        node: &hir::FragmentDefinition,
    ) -> Result<Option<apollo_encoder::FragmentDefinition>, BoxError> {
        let fragment_is_authorized = self
            .compiler
            .db
            .types_definitions_by_name()
            .get(node.type_condition())
            .is_some_and(|ty| self.is_type_authorized(ty));

        if !fragment_is_authorized {
            Ok(None)
        } else {
            transform::fragment_definition(self, node)
        }
    }

    fn fragment_spread(
        &mut self,
        node: &hir::FragmentSpread,
    ) -> Result<Option<apollo_encoder::FragmentSpread>, BoxError> {
        let fragments = self.compiler.db.fragments(self.file_id);
        let condition = fragments
            .get(node.name())
            .ok_or("MissingFragmentDefinition")?
            .type_condition();
        self.current_path
            .push(PathElement::Fragment(condition.into()));

        let fragment_is_authorized = self
            .compiler
            .db
            .types_definitions_by_name()
            .get(condition)
            .is_some_and(|ty| self.is_type_authorized(ty));

        let res = if !fragment_is_authorized {
            self.query_requires_scopes = true;
            self.unauthorized_paths.push(self.current_path.clone());

            Ok(None)
        } else {
            transform::fragment_spread(self, node)
        };

        self.current_path.pop();
        res
    }

    fn inline_fragment(
        &mut self,
        parent_type: &str,

        node: &hir::InlineFragment,
    ) -> Result<Option<apollo_encoder::InlineFragment>, BoxError> {
        match node.type_condition() {
            None => {
                self.current_path.push(PathElement::Fragment(String::new()));
                let res = transform::inline_fragment(self, parent_type, node);
                self.current_path.pop();
                res
            }
            Some(name) => {
                self.current_path.push(PathElement::Fragment(name.into()));

                let fragment_is_authorized = self
                    .compiler
                    .db
                    .types_definitions_by_name()
                    .get(name)
                    .is_some_and(|ty| self.is_type_authorized(ty));

                let res = if !fragment_is_authorized {
                    self.query_requires_scopes = true;
                    self.unauthorized_paths.push(self.current_path.clone());
                    Ok(None)
                } else {
                    transform::inline_fragment(self, parent_type, node)
                };

                self.current_path.pop();

                res
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::collections::HashSet;

    use apollo_compiler::ApolloCompiler;
    use apollo_encoder::Document;

    use crate::json_ext::Path;
    use crate::plugins::authorization::scopes::ScopeExtractionVisitor;
    use crate::plugins::authorization::scopes::ScopeFilteringVisitor;
    use crate::spec::query::transform;
    use crate::spec::query::traverse;

    static BASIC_SCHEMA: &str = r#"
    directive @requiresScopes(scopes: [String]) on OBJECT | FIELD_DEFINITION | INTERFACE | SCALAR | ENUM

    type Query {
      topProducts: Product
      customer: User
      me: User @requiresScopes(scopes: ["profile"])
      itf: I
    }

    type Mutation {
        ping: User @requiresScopes(scopes: ["ping"])
    }

    interface I {
        id: ID
    }

    type Product {
      type: String
      price(setPrice: Int): Int
      reviews: [Review]
      internal: Internal
      publicReviews: [Review]
    }

    scalar Internal @requiresScopes(scopes: ["internal", "test"]) @specifiedBy(url: "http///example.com/test")

    type Review @requiresScopes(scopes: ["review"]) {
        body: String
        author: User
    }

    type User implements I @requiresScopes(scopes: ["read:user"]) {
      id: ID
      name: String @requiresScopes(scopes: ["read:username"])
    }
    "#;

    fn extract(query: &str) -> BTreeSet<String> {
        let mut compiler = ApolloCompiler::new();

        let _schema_id = compiler.add_type_system(BASIC_SCHEMA, "schema.graphql");
        let id = compiler.add_executable(query, "query.graphql");

        let diagnostics = compiler.validate();
        for diagnostic in &diagnostics {
            println!("{diagnostic}");
        }
        assert!(diagnostics.is_empty());

        let mut visitor = ScopeExtractionVisitor::new(&compiler);
        traverse::document(&mut visitor, id).unwrap();

        visitor.extracted_scopes.into_iter().collect()
    }

    #[test]
    fn extract_scopes() {
        static QUERY: &str = r#"
        {
            topProducts {
                type
                internal
            }

            me {
                name
            }
        }
        "#;

        let doc = extract(QUERY);

        insta::assert_debug_snapshot!(doc);
    }

    fn filter(query: &str, scopes: HashSet<String>) -> (Document, Vec<Path>) {
        let mut compiler = ApolloCompiler::new();

        let _schema_id = compiler.add_type_system(BASIC_SCHEMA, "schema.graphql");
        let file_id = compiler.add_executable(query, "query.graphql");

        let diagnostics = compiler.validate();
        for diagnostic in &diagnostics {
            println!("{diagnostic}");
        }
        assert!(diagnostics.is_empty());

        let mut visitor = ScopeFilteringVisitor::new(&compiler, file_id, scopes);
        (
            transform::document(&mut visitor, file_id).unwrap(),
            visitor.unauthorized_paths,
        )
    }

    #[test]
    fn filter_basic_query() {
        static QUERY: &str = r#"
        {
            topProducts {
                type
                internal
            }

            me {
                id
                name
            }
        }
        "#;

        let (doc, paths) = filter(QUERY, HashSet::new());
        insta::assert_display_snapshot!(doc);
        insta::assert_debug_snapshot!(paths);

        let (doc, paths) = filter(
            QUERY,
            ["profile".to_string(), "internal".to_string()]
                .into_iter()
                .collect(),
        );
        insta::assert_display_snapshot!(doc);
        insta::assert_debug_snapshot!(paths);

        let (doc, paths) = filter(
            QUERY,
            [
                "profile".to_string(),
                "read:user".to_string(),
                "internal".to_string(),
                "test".to_string(),
            ]
            .into_iter()
            .collect(),
        );
        insta::assert_display_snapshot!(doc);
        insta::assert_debug_snapshot!(paths);

        let (doc, paths) = filter(
            QUERY,
            [
                "profile".to_string(),
                "read:user".to_string(),
                "read:username".to_string(),
            ]
            .into_iter()
            .collect(),
        );
        insta::assert_display_snapshot!(doc);
        insta::assert_debug_snapshot!(paths);
    }

    #[test]
    fn mutation() {
        static QUERY: &str = r#"
        mutation {
            ping {
                name
            }
        }
        "#;

        let (doc, paths) = filter(QUERY, HashSet::new());

        insta::assert_display_snapshot!(doc);
        insta::assert_debug_snapshot!(paths);
    }

    #[test]
    fn query_field() {
        static QUERY: &str = r#"
        query {
            topProducts {
                type
            }

            me {
                name
            }
        }
        "#;

        let (doc, paths) = filter(QUERY, HashSet::new());

        insta::assert_display_snapshot!(doc);
        insta::assert_debug_snapshot!(paths);
    }

    #[test]
    fn scalar() {
        static QUERY: &str = r#"
        query {
            topProducts {
                type
                internal
            }
        }
        "#;

        let (doc, paths) = filter(QUERY, HashSet::new());

        insta::assert_display_snapshot!(doc);
        insta::assert_debug_snapshot!(paths);
    }

    #[test]
    fn array() {
        static QUERY: &str = r#"
        query {
            topProducts {
                type
                publicReviews {
                    body
                    author {
                        name
                    }
                }
            }
        }
        "#;

        let (doc, paths) = filter(QUERY, HashSet::new());

        insta::assert_display_snapshot!(doc);
        insta::assert_debug_snapshot!(paths);
    }

    #[test]
    fn interface_inline_fragment() {
        static QUERY: &str = r#"
        query {
            topProducts {
                type
            }
            itf {
                id
                ... on User {
                    name
                }
            }
        }
        "#;

        let (doc, paths) = filter(QUERY, HashSet::new());

        insta::assert_display_snapshot!(doc);
        insta::assert_debug_snapshot!(paths);
    }

    #[test]
    fn interface_fragment() {
        static QUERY: &str = r#"
        query {
            topProducts {
                type
            }
            itf {
                id
                ...F
            }
        }

        fragment F on User {
            name
        }
        "#;

        let (doc, paths) = filter(QUERY, HashSet::new());

        insta::assert_display_snapshot!(doc);
        insta::assert_debug_snapshot!(paths);

        let (doc, paths) = filter(
            QUERY,
            ["read:user".to_string(), "read:username".to_string()]
                .into_iter()
                .collect(),
        );

        insta::assert_display_snapshot!(doc);
        insta::assert_debug_snapshot!(paths);
    }
}
