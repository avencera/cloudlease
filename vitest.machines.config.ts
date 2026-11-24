import { AWS_BINDINGS, LATITUDE_BINDINGS, machinesConfig, PHOENIXNAP_BINDINGS } from "./vitest.machines.shared";

export default machinesConfig(["apps/machines/test/**/*.worker.test.ts"], {
  ...PHOENIXNAP_BINDINGS,
  ...LATITUDE_BINDINGS,
  ...AWS_BINDINGS,
});
