// the same Worker with no phoenixNAP or Latitude.sh credentials, whose classes are refused
import { machinesConfig } from "./vitest.machines.shared";

export default machinesConfig(["apps/machines/test/**/*.bare.test.ts"], {});
