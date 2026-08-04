import glob, json

files = glob.glob("/Users/antonhahn/FlashOS/components/flash/crates/**/*.rs", recursive=True)
files += ["/Users/antonhahn/FlashOS/components/flash/README.md", "/Users/antonhahn/FlashOS/components/flash/CHANGELOG.md"]

out = []
for path in files:
    with open(path, "r") as f:
        lines = f.readlines()
        
    chunks = []
    for i, line in enumerate(lines):
        if "flashshell" in line.lower() or "FlashShell" in line:
            new_line = line.replace("FlashShell", "Flash").replace("flashshell", "flash")
            chunks.append({
                "AllowMultiple": True,
                "StartLine": i + 1,
                "EndLine": i + 1,
                "TargetContent": line,
                "ReplacementContent": new_line
            })
    if chunks:
        out.append({
            "TargetFile": path,
            "Instruction": "Rename flashshell to flash",
            "Description": "Rename flashshell to flash",
            "ReplacementChunks": chunks,
            "toolSummary": f"Edit {path.split('/')[-1]}",
            "toolAction": f"Editing {path.split('/')[-1]}"
        })

for i, call in enumerate(out):
    args = json.dumps(call)
    print(f'\\u003c!-- CALL {i} --\\u003e')
    print(f'\\u003ccall:default_api:multi_replace_file_content{args}\\u003e')

print(f"Total calls: {len(out)}")
