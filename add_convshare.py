with open('G:/ZCode-main/native/crates/nc-code-app/src/lib.rs') as f:
    c = f.read()
if 'conversation_share' not in c:
    c = c.replace('use nc_code_services_system::SystemService;\n', 
                  'use nc_code_services_system::SystemService;\nuse nc_code_services_conversation_share::ConversationShareService;\n')
if '"conversation-share"' not in c:
    c = c.replace('    registry.register("system", SystemService::new())?;\n',
                  '    registry.register("system", SystemService::new())?;\n    registry.register("conversation-share", ConversationShareService::new())?;\n')
with open('G:/ZCode-main/native/crates/nc-code-app/src/lib.rs', 'w') as f:
    f.write(c)
print('done')
