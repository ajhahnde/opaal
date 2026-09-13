# Inspect a task or artifact

Use the inspection form that matches what you need to review:

```sh
opaal task inspect --project opaal.toml welcome
opaal plan inspect welcome.plan.json
opaal audit inspect welcome.audit.json
```

Task inspection reads the explicit project source closure and reports the action signature, effects, tools, and environments without selecting authority or running the action. Plan inspection validates one canonical plan and prints a bounded redacted human view, including its outcome and bound digest. It rejects old, future, tampered, or invalid artifacts. Audit inspection validates the audit's own schema and digest and renders its recorded prefix identity and evidence.

Audit inspection does not reread journal bytes; `opaal audit` is the command that validates the journal. There is no journal-inspection command. For the full sequence see [review and execute](review-and-execute-a-plan.md) and [audit a run](audit-a-run.md).
