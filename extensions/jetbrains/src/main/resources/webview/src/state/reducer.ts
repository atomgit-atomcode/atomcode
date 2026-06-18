import type { UiState, UiAction } from './types';

export const initialUiState: UiState = {
  model: null,
  isAutoScrolling: true,
};

export function uiReducer(state: UiState, action: UiAction): UiState {
  switch (action.type) {
    case 'FULL_SYNC':
      return {
        ...state,
        model: action.model,
      };
    case 'SET_AUTO_SCROLL':
      return { ...state, isAutoScrolling: action.value };
    default:
      return state;
  }
}
