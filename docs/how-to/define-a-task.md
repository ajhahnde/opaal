# Define a task

Write an action in the manifest's root module, then export it with one `task` declaration:

```opaal
action greet(name: String) -> String
effects {
}
{
    return name
}
task welcome = greet
```

The task inherits the action's signature and declared effects. It cannot override either. A task in an imported module is invalid; a standalone source file cannot declare one. Inspect before adding authority:

```sh
opaal task inspect --project opaal.toml welcome
```

For an effectful action, put static requests in `effects { … }` and import the required `project::` identity explicitly. A caller must declare the effects of every action it calls. The declaration alone does not grant access: choose an environment with exact authority rows, check the task, then plan and review it. [Actions and effects](../reference/language/actions-and-effects.md) lists the closed families.
