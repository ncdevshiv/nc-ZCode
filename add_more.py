with open('G:/ZCode-main/native/crates/nc-code-app/src/lib.rs') as f:
    c = f.read()

# imports
mapping = [('commands','CommandsService'),('hooks','HooksService'),('memory','MemoryService'),('subagents','SubagentsService')]
for name, struct in mapping:
    imp = f'use nc_code_services_{name}::{struct};\n'
    if struct not in c:
        c = c.replace('use nc_code_services_conversation_share::ConversationShareService;\n', 
                      'use nc_code_services_conversation_share::ConversationShareService;\n' + imp)

if 'CommandsService' not in c and '"conversation-share"' in c:
    c = c.replace('    registry.register("conversation-share", ConversationShareService::new())?;\n',
                  '    registry.register("conversation-share", ConversationShareService::new())?;\n    registry.register("commands", CommandsService::new())?;\n    registry.register("hooks", HooksService::new())?;\n    registry.register("memory", MemoryService::new())?;\n    registry.register("subagents", SubagentsService::new())?;\n')

with open('G:/ZCode-main/native/crates/nc-code-app/src/lib.rs', 'w') as f:
    f.write(c)
print('done')
