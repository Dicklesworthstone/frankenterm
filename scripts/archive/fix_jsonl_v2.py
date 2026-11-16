import json

def fix_utf8_jsonl(filename):
    with open(filename, 'rb') as f:
        content = f.read()
    
    # decode with ignore
    text = content.decode('utf-8', errors='ignore')
    
    valid_lines = []
    broken_count = 0
    
    for i, line in enumerate(text.splitlines(keepends=True)):
        if not line.strip():
            continue
        try:
            json.loads(line)
            valid_lines.append(line)
        except json.JSONDecodeError as e:
            print(f"Line {i+1} broken: {e}")
            broken_count += 1

    if broken_count > 0:
        with open(filename, 'w', encoding='utf-8') as f:
            f.writelines(valid_lines)
        print(f"Fixed {filename}, removed {broken_count} broken lines.")
    else:
        print("No broken lines found.")

fix_utf8_jsonl('.beads/issues.jsonl')
