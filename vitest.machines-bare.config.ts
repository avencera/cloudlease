// the same Worker with no phoenixNAP credentials, whose phoenixNAP classes are refused
import { machinesConfig } from "./vitest.machines.shared";

export default machinesConfig(["apps/machines/test/**/*.bare.test.ts"], {});
