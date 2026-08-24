from pathlib import Path
p = Path('src/components/workspace/WorkspaceManager.tsx')
content = p.read_bytes()
lines = content.split(b'\n')

# Find select
for i, l in enumerate(lines):
    if b'<select' in l:
        for j in range(i, min(i+30, len(lines))):
            if b'</select>' in lines[j]:
                start, end = i, j
                break
        break

new_block = b'''                    <Dropdown
                      value={dir.access}
                      onChange={(v) => handleAccessChange(dir, v as "read-only" | "read-write")}
                      className={`rounded-md border px-2 py-1.5 text-xs font-medium ${dir.access === "read-write"
                        ? "border-orange-300 bg-orange-50 text-orange-700 dark:border-orange-700 dark:bg-orange-900/30 dark:text-orange-400"
                        : "border-zinc-300 bg-zinc-50 text-zinc-700 dark:border-zinc-600 dark:bg-zinc-800 dark:text-zinc-300"
                        }`}
                      options={[
                        { value: "read-only", label: "\xf0\x9f\x94\x92 Read-only" },
                        { value: "read-write", label: "\xe2\x9c\x8f Read-write" },
                      ]}
                    />'''

new_lines = lines[:start] + [new_block] + lines[end+1:]
p.write_bytes(b'\n'.join(new_lines))
print('done, total lines:', len(new_lines))
