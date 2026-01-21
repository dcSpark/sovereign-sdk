import type { HealthResponse, ActionType, ActionResult } from './types';

const API_BASE = '/api';

export async function fetchHealth(): Promise<HealthResponse> {
  const response = await fetch(`${API_BASE}/health`);
  if (!response.ok) {
    throw new Error(`Health check failed: ${response.statusText}`);
  }
  return response.json();
}

export async function performAction(action: ActionType): Promise<ActionResult> {
  try {
    const response = await fetch(`${API_BASE}/${action}`, {
      method: 'POST',
    });
    const message = await response.text();
    return {
      success: response.ok,
      message: message || (response.ok ? 'Success' : 'Failed'),
    };
  } catch (error) {
    return {
      success: false,
      message: error instanceof Error ? error.message : 'Unknown error',
    };
  }
}
