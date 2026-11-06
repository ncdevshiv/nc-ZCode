with open('G:/ZCode-main/native/crates/nc-code-services-config/src/lib.rs') as f:
    c = f.read()

if 'cua_permission' not in c:
    c = c.replace('pub mod setting;\n', 'pub mod setting;\npub mod cua_permission;\npub mod cua_pip_session;\n')
if 'CuaPermissionService' not in c:
    c = c.replace('    pub credential: Arc<credential::CredentialService>,\n}\n',
                  '    pub credential: Arc<credential::CredentialService>,\n    pub cua_permission: Arc<cua_permission::CuaPermissionService>,\n    pub cua_pip_session: Arc<cua_pip_session::CuaPipSessionService>,\n}\n')
if 'CuaPermissionService::new()' not in c:
    c = c.replace('            credential: Arc::new(credential::CredentialService::new(paths.clone())),\n            account,\n            paths,\n        }\n',
                  '            credential: Arc::new(credential::CredentialService::new(paths.clone())),\n            cua_permission: cua_permission::CuaPermissionService::new(),\n            cua_pip_session: cua_pip_session::CuaPipSessionService::new(),\n            account,\n            paths,\n        }\n')
if '"cua-permission"' not in c:
    c = c.replace('        registry.register("credential", self.credential.clone())?;\n        Ok(())\n',
                  '        registry.register("credential", self.credential.clone())?;\n        registry.register("cua-permission", self.cua_permission.clone())?;\n        registry.register("cua-pip-session", self.cua_pip_session.clone())?;\n        Ok(())\n')

with open('G:/ZCode-main/native/crates/nc-code-services-config/src/lib.rs', 'w') as f:
    f.write(c)
print('done')
