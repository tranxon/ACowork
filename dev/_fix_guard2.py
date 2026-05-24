p = 'core/rollball-gateway/src/http/agents.rs'
with open(p, encoding='utf-8') as f:
    c = f.read()

c = c.replace("ApiError::precondition", "ApiError::service_unavailable")
# The function name is the same, only the method name changes.

with open(p, 'w', encoding='utf-8') as f:
    f.write(c)
print('ok')
