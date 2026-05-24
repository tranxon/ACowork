import os

base = r'core'

# ============================================
# Fix 1: cli.rs - remove orphaned 'e {' block
# ============================================
f = os.path.join(base, 'rollball-runtime', 'src', 'cli.rs')
with open(f, encoding='utf-8') as fh:
    c = fh.read()

# Find the orphaned block: 'e {' right after the noop provider block
# The noop block ends with: ProtocolType::OpenAI)\n    };\n\ne {
idx_marker = c.find('ProtocolType::OpenAI)\n    };\n\ne {')
if idx_marker > 0:
    # Find closing '    };' of orphan block
    orphan_start = idx_marker + len('ProtocolType::OpenAI)\n    };\n\n')
    orphan_end = c.find('\n    };\n\n    // Step 4:', orphan_start)
    if orphan_end < 0:
        orphan_end = c.find('\n    };\n\n    // Step 4', orphan_start)
    if orphan_end > 0:
        c = c[:orphan_start] + c[orphan_end + 8:]  # skip '\n    };\n\n'
        print('Fix 1: cli.rs orphaned block removed')
    else:
        print('Fix 1: closing bracket not found')
else:
    # Try alternative pattern
    idx_marker = c.find('OpenAI)\n    };\n\ne {')
    if idx_marker > 0:
        orphan_start = idx_marker + len('OpenAI)\n    };\n\n')
        orphan_end = c.find('\n    };\n\n    // Step 4:', orphan_start)
        if orphan_end > 0:
            c = c[:orphan_start] + c[orphan_end + 8:]
            print('Fix 1 (alt): cli.rs orphaned block removed')
        else:
            print('Fix 1 (alt): closing bracket not found')
    else:
        print('Fix 1: marker NOT found at all')

with open(f, 'w', encoding='utf-8') as fh:
    fh.write(c)

# ============================================
# Fix 2: grpc/client.rs - GatewayRequest version fields
# ============================================
f = os.path.join(base, 'rollball-runtime', 'src', 'grpc', 'client.rs')
with open(f, encoding='utf-8') as fh:
    c = fh.read()

old = """        let request = GatewayRequest::AgentHello {
            agent_id: agent_id.to_string(),
            version: version.to_string(),
            connection_role: connection_role.to_string(),
        };"""
new = """        let request = GatewayRequest::AgentHello {
            agent_id: agent_id.to_string(),
            version: version.to_string(),
            connection_role: connection_role.to_string(),
            provider_list_version: 0,
            mcp_list_version: 0,
        };"""
if old in c:
    c = c.replace(old, new)
    print('Fix 2: GatewayRequest version fields added')
else:
    print('Fix 2: Pattern NOT found')

with open(f, 'w', encoding='utf-8') as fh:
    fh.write(c)

print('All fixes applied')
