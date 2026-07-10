export interface ModelStateEvent {
  event_type: string;
  model_id?: string;
  model_name?: string;
  error?: string;
}

export interface RecordingErrorEvent {
  error_type: string;
  detail?: string;
}

/** 后端转写管线失败时的事件载荷("transcription-error"),error 为原始错误信息。 */
export interface TranscriptionErrorEvent {
  error: string;
}
