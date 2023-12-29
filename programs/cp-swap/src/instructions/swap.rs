use crate::curve::TradeDirection;
use crate::error::ErrorCode;
use crate::states::*;
use crate::utils::token::*;
use anchor_lang::prelude::*;
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};

use crate::curve::calculator::CurveCalculator;
/// Memo msg for swap
pub const SWAP_MEMO_MSG: &'static [u8] = b"raydium_token_swap_memo";
#[derive(Accounts)]
pub struct Swap<'info> {
    /// The user performing the swap
    pub payer: Signer<'info>,

    /// The factory state to read protocol fees
    #[account(address = pool_state.load()?.amm_config)]
    pub amm_config: Box<Account<'info, AmmConfig>>,

    /// The program account of the pool in which the swap will be performed
    #[account(mut)]
    pub pool_state: AccountLoader<'info, PoolState>,

    /// The user token account for input token
    #[account(mut)]
    pub input_token_account: Box<InterfaceAccount<'info, TokenAccount>>,

    /// The user token account for output token
    #[account(mut)]
    pub output_token_account: Box<InterfaceAccount<'info, TokenAccount>>,

    /// The vault token account for input token
    #[account(
        mut,
        constraint = input_vault.key() == pool_state.load()?.token_0_vault || input_vault.key() == pool_state.load()?.token_1_vault
    )]
    pub input_vault: Box<InterfaceAccount<'info, TokenAccount>>,

    /// The vault token account for output token
    #[account(
        mut,
        constraint = output_vault.key() == pool_state.load()?.token_0_vault || output_vault.key() == pool_state.load()?.token_1_vault
    )]
    pub output_vault: Box<InterfaceAccount<'info, TokenAccount>>,

    /// SPL program for input token transfers
    pub input_token_program: Interface<'info, TokenInterface>,

    /// SPL program for output token transfers
    pub output_token_program: Interface<'info, TokenInterface>,

    /// The mint of input token
    #[account(
        address = input_vault.mint
    )]
    pub input_token_mint: Box<InterfaceAccount<'info, Mint>>,

    /// The mint of output token
    #[account(
        address = output_vault.mint
    )]
    pub output_token_mint: Box<InterfaceAccount<'info, Mint>>,

    /// CHECK:
    #[account(
        address = spl_memo::id()
    )]
    pub memo_program: UncheckedAccount<'info>,
}

pub fn swap(ctx: Context<Swap>, amount_in: u64, minimum_amount_out: u64) -> Result<()> {
    let pool_state = &mut ctx.accounts.pool_state.load_mut()?;
    if !pool_state.get_status_by_bit(PoolStatusBitIndex::Swap) {
        return err!(ErrorCode::NotApproved);
    }
    let transfer_fee = get_transfer_fee(&ctx.accounts.input_token_mint, amount_in)?;
    // Take transfer fees into account for actual amount transferred in
    let actual_amount_in = amount_in.saturating_sub(transfer_fee);

    // Calculate the trade amounts
    let (trade_direction, total_input_token_amount, total_output_token_amount) =
        if ctx.accounts.input_vault.key() == pool_state.token_0_vault {
            let (total_input_token_amount, total_output_token_amount) = pool_state
                .vault_amount_without_fee(
                    ctx.accounts.input_vault.amount,
                    ctx.accounts.output_vault.amount,
                );

            (
                TradeDirection::ZeroForOne,
                total_input_token_amount,
                total_output_token_amount,
            )
        } else {
            let (total_input_token_amount, total_output_token_amount) = pool_state
                .vault_amount_without_fee(
                    ctx.accounts.output_vault.amount,
                    ctx.accounts.input_vault.amount,
                );

            (
                TradeDirection::OneForZero,
                total_input_token_amount,
                total_output_token_amount,
            )
        };
    let constant_before = u128::from(total_input_token_amount)
        .checked_mul(u128::from(total_output_token_amount))
        .unwrap();

    let result = CurveCalculator::swap(
        u128::from(actual_amount_in),
        u128::from(total_input_token_amount),
        u128::from(total_output_token_amount),
        trade_direction,
        ctx.accounts.amm_config.trade_fee_rate,
        ctx.accounts.amm_config.protocol_fee_rate,
        ctx.accounts.amm_config.fund_fee_rate,
    )
    .ok_or(ErrorCode::ZeroTradingTokens)?;

    let constant_after = u128::from(result.new_swap_source_amount)
        .checked_mul(u128::from(result.new_swap_destination_amount))
        .unwrap();
    require_gte!(constant_after, constant_before);

    // Re-calculate the source amount swapped based on what the curve says
    let input_transfer_amount = {
        let source_amount_swapped = u64::try_from(result.source_amount_swapped).unwrap();
        let transfer_fee =
            get_transfer_inverse_fee(&ctx.accounts.input_token_mint, source_amount_swapped)?;
        source_amount_swapped.checked_add(transfer_fee).unwrap()
    };

    let output_transfer_amount = {
        let amount_out = u64::try_from(result.destination_amount_swapped).unwrap();
        let transfer_fee = get_transfer_fee(&ctx.accounts.output_token_mint, amount_out)?;
        let amount_received = amount_out.checked_sub(transfer_fee).unwrap();
        if amount_received < minimum_amount_out {
            return Err(ErrorCode::ExceededSlippage.into());
        }
        amount_out
    };

    let protocol_fee = u64::try_from(result.protocol_fee).unwrap();
    let fund_fee = u64::try_from(result.fund_fee).unwrap();

    match trade_direction {
        TradeDirection::ZeroForOne => {
            pool_state.protocol_fees_token_0 = pool_state
                .protocol_fees_token_0
                .checked_add(protocol_fee)
                .unwrap();
            pool_state.fund_fees_token_0 =
                pool_state.fund_fees_token_0.checked_add(fund_fee).unwrap();
        }
        TradeDirection::OneForZero => {
            pool_state.protocol_fees_token_1 = pool_state
                .protocol_fees_token_1
                .checked_add(protocol_fee)
                .unwrap();
            pool_state.fund_fees_token_1 =
                pool_state.fund_fees_token_1.checked_add(fund_fee).unwrap();
        }
    };

    transfer_from_user_to_pool_vault(
        ctx.accounts.payer.to_account_info(),
        ctx.accounts.input_token_account.to_account_info(),
        ctx.accounts.input_vault.to_account_info(),
        ctx.accounts.input_token_mint.to_account_info(),
        ctx.accounts.input_token_program.to_account_info(),
        input_transfer_amount,
        ctx.accounts.input_token_mint.decimals,
    )?;

    transfer_from_pool_vault_to_user(
        &ctx.accounts.pool_state,
        ctx.accounts.output_vault.to_account_info(),
        ctx.accounts.output_token_account.to_account_info(),
        ctx.accounts.output_token_mint.to_account_info(),
        ctx.accounts.output_token_program.to_account_info(),
        output_transfer_amount,
        ctx.accounts.output_token_mint.decimals,
    )?;

    Ok(())
}
