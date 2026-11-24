// the same Worker with no phoenixNAP, Latitude.sh or AWS credentials, whose classes are refused
import { machinesConfig } from "./vitest.machines.shared";

export default machinesConfig(["apps/machines/test/**/*.bare.test.ts"], {});
