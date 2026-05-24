import re
import os
base = r'core'

# ── Fix all remaining compilation errors ──

# 1. GatewayRequest::AgentHello needs version fields (grpc/client.rs line 408)
f = os.path.join(base, 'rollball-runtime', 'src', 'grpc', 'client.rs')
with open(f, encoding='utf-8') as fh:
    c = fh.read()

old = '''        let request = GatewayRequest::AgentHello {
            agent_id: agent_id.to_string(),
            version: version.to_string(),
            connection_role: connection_role.to_string(),
        };'''
new = '''        let request = GatewayRequest::AgentHello {
            agent_id: agent_id.to_string(),
            version: version.to_string(),
            connection_role: connection_role.to_string(),
            provider_list_version: 0,
            mcp_list_version: 0,
        };'''
c = c.replace(old, new)
print('1. GatewayRequest::AgentHello version fields added')

# 2. Find remaining proto AgentHelloResult field accesses
# Search for result.provider, result.model, etc.
old_patterns = [
    ('result.provider', 'result.provider_list_json /* FIXME(Task6) */'),
    ('result.model', '"" /* FIXME(Task6) */'),
    ('result.api_key', '"" /* FIXME(Task6) */'),
    ('result.base_url', '"" /* FIXME(Task6) */'),
    ('result.models', 'Vec::new() /* FIXME(Task6) */'),
    ('result.model_capabilities', 'None /* FIXME(Task6) */'),
    ('result.max_output_tokens_limit', '32768 /* FIXME(Task6) */'),
    ('result.protocol_type', '"OpenAI".to_string() /* FIXME(Task6) */'),
]
for old_p, new_p in old_patterns:
    if old_p in c:
        c = c.replace(old_p, new_p)
        print(f'  Replaced: {old_p}')
    old_p_meth = '.' + old_p.split('.')[-1] + '('  # Handle method calls
print('2. Proto field refs checked')

with open(f, 'w', encoding='utf-8') as fh:
    fh.write(c)

# 3. cli.rs - find and fix the old cfg.provider etc in the hello_config 2nd block (line ~1679 area)
f = os.path.join(base, 'rollball-runtime', 'src', 'cli.rs')
with open(f, encoding='utf-8') as fh:
    c = fh.read()

# Search for remaining 'cfg.' references
matches = re.findall(r'cfg\.\w+', c)
if matches:
    print(f'3. Remaining cfg. references in cli.rs: {set(matches)}')
else:
    print('3. No remaining cfg.* in cli.rs')

# Find and fix code at line ~1720 area where save_agent_model uses cfg.provider
old_save = '''                // Preserve the user\'s provider preference from agent_model.json
                // when available; otherwise fall back to Gateway-resolved provider.
                let provider_to_save = persisted_provider.as_deref(); // FIXME(Task10): provider from agent_model.json
                save_agent_model(&config.work_dir, &resolved_model, provider_to_save);'''
if old_save in c:
    print('3. save_agent_model block OK')
else:
    # Find it
    idx = c.find('save_agent_model(&config.work_dir, &resolved_model,')
    if idx > 0:
        print(f'3. save_agent_model found at offset {idx}')
    else:
        print('3. save_agent_model NOT found - may have been in stubbed block')

with open(f, 'w', encoding='utf-8') as fh:
    fh.write(c)
print('Done fixing')
