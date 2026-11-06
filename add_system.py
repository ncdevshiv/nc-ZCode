with open('G:/ZCode-main/native/crates/nc-code-app/src/lib.rs', 'r') as f:
    content = f.read()

# Add import
if 'nc_code_services_system' not in content:
    content = content.replace('use nc_code_services_workspace::WorkspaceServices;\n',
        'use nc_code_services_workspace::WorkspaceServices;\nuse nc_code_services_system::SystemService;\n')

# Add registration before off-peak
if '"system"' not in content:
    old = '''    // off-peak-task: cloud scheduling is disabled per user policy (specs/disabled-features.md §2.1).
    // The channel registers in disabled form; the frontend hides the entry based on feature_disabled from getCodingPlanSupport.
    registry.register("off-peak-task", DisabledOffPeakTaskService::new())?;'''
    new = '''    // system service
    registry.register("system", SystemService::new())?;
    // off-peak-task: cloud scheduling is disabled per user policy (specs/disabled-features.md §2.1).
    // The channel registers in disabled form; the frontend hides the entry based on feature_disabled from getCodingPlanSupport.
    registry.register("off-peak-task", DisabledOffPeakTaskService::new())?;'''
    content = content.replace(old, new)

with open('G:/ZCode-main/native/crates/nc-code-app/src/lib.rs', 'w') as f:
    f.write(content)

print('done')
