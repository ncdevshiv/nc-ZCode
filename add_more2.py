with open('G:/ZCode-main/native/crates/nc-code-app/src/lib.rs') as f:
    c = f.read()

for name, struct in [('feedback','FeedbackService'),('settings_sync','SettingsSyncService')]:
    imp = f'use nc_code_services_{name}::{struct};\n'
    if struct not in c:
        c = c.replace('use nc_code_services_subagents::SubagentsService;\n', 
                      'use nc_code_services_subagents::SubagentsService;\n' + imp)

if 'FeedbackService' not in c and '"subagents"' in c:
    c = c.replace('    registry.register("subagents", SubagentsService::new())?;\n',
                  '    registry.register("subagents", SubagentsService::new())?;\n    registry.register("feedback", FeedbackService::new())?;\n    registry.register("settings-sync", SettingsSyncService::new())?;\n')

with open('G:/ZCode-main/native/crates/nc-code-app/src/lib.rs', 'w') as f:
    f.write(c)
print('done')
