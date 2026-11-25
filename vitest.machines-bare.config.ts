// the same Worker with no phoenixNAP or Latitude.sh credentials, whose classes are refused,
// and AWS settings broken by an access key set without its secret
import { machinesConfig } from "./vitest.machines.shared";

export default machinesConfig(["apps/machines/test/**/*.bare.test.ts"], {
  AWS_ACCESS_KEY_ID: "test-aws-key",
});
