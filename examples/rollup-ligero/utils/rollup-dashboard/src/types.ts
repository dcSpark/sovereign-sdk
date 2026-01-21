export interface ServiceHealth {
  name: string;
  url: string;
  status: string;
  error?: string;
  response_time_ms?: number;
}

export interface HealthResponse {
  status: string;
  services: ServiceHealth[];
  checkedAt: string;
}

export type ActionType = 'start' | 'stop' | 'restart' | 'clean';

export interface ActionResult {
  success: boolean;
  message: string;
}
