#![forbid(unsafe_code)]

#[path = "support/formatting.rs"]
mod formatting;

use opaal_syntax::{
    ExpressionKind, ParseOutcome, Pattern, RecordKey, SourceFile, SourceId, StatementKind,
    parse_opaal,
};

fn source(text: &str) -> SourceFile {
    SourceFile::new(SourceId::new(81), "record.opaal", text)
}

#[test]
fn shorthand_lowers_to_existing_nodes_at_the_original_token() {
    let file = source(
        "let fields = {name, port: 7}\nlet server = model::Server {name, port: 7}\nlet Server {name, port: selected, _} = server\n",
    );
    let ParseOutcome::Complete(script) = parse_opaal(&file) else {
        panic!("{:?}", parse_opaal(&file));
    };
    for statement in &script.statements()[..2] {
        let StatementKind::Declaration(declaration) = statement.kind() else {
            panic!()
        };
        let (key, value) = match declaration.value.kind() {
            ExpressionKind::Record(entries) => {
                let RecordKey::Identifier(key) = entries[0].key else {
                    panic!()
                };
                (key, &entries[0].value)
            }
            ExpressionKind::NominalRecord(record) => {
                (record.fields[0].name, &record.fields[0].value)
            }
            _ => panic!(),
        };
        let ExpressionKind::Name(reference) = value.kind() else {
            panic!()
        };
        assert_eq!(reference.name, key);
        assert_eq!(value.span(), key.span());
        assert_eq!(file.slice(value.span()).unwrap(), "name");
    }
    let StatementKind::Declaration(declaration) = script.statements()[2].kind() else {
        panic!()
    };
    let Pattern::NominalRecord(pattern) = &declaration.pattern else {
        panic!()
    };
    assert_eq!(
        pattern.fields[0].pattern,
        Pattern::Binding(pattern.fields[0].name)
    );
    assert_eq!(
        pattern.fields[2].pattern,
        Pattern::Wildcard(pattern.fields[2].name.span())
    );
}

#[test]
fn formatting_preserves_spelling_comments_and_control_command_boundaries() {
    for text in [
        "let x={name,port:port}\nlet Server{name,port:other}=server\n",
        "let x = Server {name # 界🙂\n, port: port,}\n",
        "if ready { worker }\nwhile ready { worker }\nfor item in values { worker }\nmatch value { selected => { worker } }\n",
        "if accepts(Server {name}) { worker }\nfor item in values(Server {name}) { worker }\nmatch identity(Server {name}) { selected => { worker } }\n",
        "if Server {name: name} { worker }\nServer {name}\n^echo {name}\n^echo \"{Server {name}}\"\n",
        "type lower = {name: Int}\nlet x = lower {name}\nlet x = {}\nlet x = lower {}\n",
        "let fields = {'界': name, \"name\": name, name}\n",
        "def pick(Server {name}: Server) { return name }\nlet pick = {|Server {name}| name}\n",
    ] {
        formatting::assert_roundtrip(&source(text));
    }
    for count in [1, 2, 64] {
        let fields = (0..count)
            .map(|n| format!("field_{n}"))
            .collect::<Vec<_>>()
            .join(", ");
        formatting::assert_roundtrip(&source(&format!("let x = {{{fields}}}\n")));
    }
}

#[test]
fn only_identifier_fields_followed_by_a_separator_can_omit_the_value() {
    for text in [
        "let x = {'name'}",
        "let x = {\"name\"}",
        "let x = {true}",
        "let x = {name.other}",
        "let x = {name other}",
        "let x = {界}",
        "let x = Server {name other}",
        "let Server {name other} = value",
        "for Server {name} in values { null }",
        "try { null } catch Server {name} { null }",
        "let {name} = value",
    ] {
        assert!(
            matches!(parse_opaal(&source(text)), ParseOutcome::Invalid(_)),
            "{text}"
        );
    }
    for text in ["let x = {name", "let x = Server {name,", "let Server {name"] {
        assert!(
            matches!(parse_opaal(&source(text)), ParseOutcome::Incomplete(_)),
            "{text}"
        );
    }
}
