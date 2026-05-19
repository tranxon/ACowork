"""Patch routes.rs to add approval routes"""
path = r"d:\projects\rust\agent-study\core\rollball-gateway\src\http\routes.rs"
with open(path, "r", encoding="utf-8") as f:
    content = f.read()

old = """        .merge(crate::http::workspaces::workspace_routes())
        .merge(crate::http::publish_api::publish_routes())
        .with_state(state)"""
new = """        .merge(crate::http::workspaces::workspace_routes())
        .merge(crate::http::publish_api::publish_routes())
        .merge(crate::http::approval::approval_routes())
        .with_state(state)"""

count = content.count(old)
content = content.replace(old, new)
print(f"Found {count} occurrence(s)")

with open(path, "w", encoding="utf-8") as f:
    f.write(content)
print("Done")
