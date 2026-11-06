import io

# ---------------------------------------------- service interface + local factory
path = 'packages/services/src/model-provider/providerFacadeServices.ts'
s = io.open(path, encoding='utf-8').read()

old = ''' /** The test has been saved and entered into the official Model of the target Environment Registry. */
  testModelConnectivity(
    input: ProviderSettingsConnectivityRequest,
  ): Promise<ModelConnectivityResult>;
}'''
new = ''' /** The test has been saved and entered into the official Model of the target Environment Registry. */
  testModelConnectivity(
    input: ProviderSettingsConnectivityRequest,
  ): Promise<ModelConnectivityResult>;
  /** Auto fetch: Read the model directory + inference capability of this provider endpoint (Rust service implementation). */
  listRemoteModels(input: RemoteModelCatalogInput): Promise<RemoteModelCatalog>;
  /** Use the product provider harness to test the single model: tools/thinking/throughput. */
  probeRemoteModel(input: RemoteModelProbeInput): Promise<RemoteModelProbeResult>;
  /** Import directory entries in batches (configuration comes from directory facts, no empty configuration is written). */
  addRemoteModels(input: RemoteModelSelectionInput): Promise<ProviderSettingsView>;
  /** Remove models in batches (Remove action of Auto fetch panel). */
  deletePersonalModels(input: RemoteModelSelectionInput): Promise<ProviderSettingsView>;
}'''
assert old in s
s = s.replace(old, new, 1)

# imports for new types
old_import = '''} from "@zcode/provider";'''
assert old_import in s
s = s.replace(old_import, '''} from "@zcode/provider";
import type {
  RemoteModelCatalog,
  RemoteModelCatalogInput,
  RemoteModelProbeInput,
  RemoteModelProbeResult,
  RemoteModelSelectionInput,
} from "@zcode/provider";''', 1)

# local factory fail-closed implementations (native channel owns this surface)
old = ''' testModelConnectivity: async (input) => {
      await ensureReady();'''
new = ''' // The remote model directory and measured capabilities belong to the fact source of the native channel
    // (`native/crates/zcode-services-provider/src/catalog.rs`). TS local assembly does not duplicate implementation// A copy of HTTP/inference logic: here it is truthfully declared that the Environment is not equipped with this capability, and the caller (UI) will display an error.
    listRemoteModels: async () => {
      throw new Error("The current Environment is not equipped with remote model directory capabilities (provided by Rust provider-settings)");
    },
    probeRemoteModel: async () => {
      throw new Error("The current Environment is not equipped with remote model capability detection (provided by Rust provider-settings)");
    },
    addRemoteModels: async () => {
      throw new Error("The current Environment is not equipped with remote model batch import (provided by Rust provider-settings)");
    },
    deletePersonalModels: async (input) => {
      await ensureReady();
      for (const modelId of input.modelIds) {
        await facade.deletePersonalModel(input.providerId, modelId);
      }
      return facade.getView();
    },
    testModelConnectivity: async (input) => {
      await ensureReady();'''
assert old in s
s = s.replace(old, new, 1)
io.open(path, 'w', encoding='utf-8', newline='').write(s)
print('service interface extended')