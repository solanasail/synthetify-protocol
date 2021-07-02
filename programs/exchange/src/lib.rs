pub mod math;
mod utils;
use anchor_lang::prelude::*;
use anchor_spl::token::{self, Burn, MintTo, TokenAccount, Transfer};
// use manager::{AssetsList, SetAssetSupply};
use utils::*;
const SYNTHETIFY_EXCHANGE_SEED: &str = "Synthetify";
#[program]
pub mod exchange {
    use std::convert::TryInto;

    use pyth::pc::Price;

    use crate::math::{
        amount_to_discount, calculate_burned_shares, calculate_debt, calculate_max_burned_in_xusd,
        calculate_max_debt_in_usd, calculate_max_withdraw_in_usd, calculate_max_withdrawable,
        calculate_new_shares_by_rounding_up, calculate_swap_out_amount, calculate_user_debt_in_usd,
        PRICE_OFFSET,
    };

    use super::*;
    #[state(zero_copy)] // To ensure upgradability state is about 2x bigger than required
    pub struct InternalState {
        // size = 321
        //8 Account signature
        pub admin: Pubkey,                //32
        pub halted: bool,                 //1
        pub nonce: u8,                    //1
        pub debt_shares: u64,             //8
        pub collateral_token: Pubkey,     //32
        pub collateral_account: Pubkey,   //32
        pub assets_list: Pubkey,          //32
        pub collateralization_level: u32, //4   In % should range from 300%-1000%
        pub max_delay: u32,               //4   Delay bettwen last oracle update 100 blocks ~ 1 min
        pub fee: u32,                     //4   Default fee per swap 300 => 0.3%
        pub liquidation_account: Pubkey,  //32
        pub liquidation_penalty: u8,      //1   In % range 0-25%
        pub liquidation_threshold: u8,    //1   In % should range from 130-200%
        pub liquidation_buffer: u32,      //4   Time given user to fix collateralization ratio
        pub account_version: u8,          //1 Version of account supported by program
        pub staking: Staking,             //116
    }
    impl InternalState {
        pub fn new(
            &mut self,
            ctx: Context<New>,
            _nonce: u8,
            _staking_round_length: u32,
            _amount_per_round: u64,
        ) -> ProgramResult {
            let slot = Clock::get()?.slot;
            self.admin = *ctx.accounts.admin.key;
            self.halted = false;
            self.nonce = _nonce;
            self.debt_shares = 0u64;
            self.assets_list = *ctx.accounts.assets_list.key;
            self.liquidation_account = *ctx.accounts.liquidation_account.key;
            self.collateralization_level = 1000;
            // once we will not be able to fit all data into one transaction we will
            // use max_delay to allow split updating oracles and exchange operation
            self.max_delay = 0;
            self.fee = 300;
            self.liquidation_penalty = 15;
            self.liquidation_threshold = 200;
            self.liquidation_buffer = 172800; // about 24 Hours;
            self.account_version = 0;
            self.staking = Staking {
                round_length: _staking_round_length,
                amount_per_round: _amount_per_round,
                fund_account: *ctx.accounts.staking_fund_account.to_account_info().key,
                finished_round: StakingRound {
                    all_points: 0,
                    amount: 0,
                    start: 0,
                },
                current_round: StakingRound {
                    all_points: 0,
                    amount: 0,
                    start: slot,
                },
                next_round: StakingRound {
                    all_points: 0,
                    amount: _amount_per_round,
                    start: slot.checked_add(_staking_round_length.into()).unwrap(),
                },
            };
            Ok(())
        }
        #[access_control(halted(&self)
        version(&self,&ctx.accounts.exchange_account)
        assets_list(&self,&ctx.accounts.assets_list))]
        pub fn deposit(&mut self, ctx: Context<Deposit>, amount: u64) -> Result<()> {
            msg!("Synthetify: DEPOSIT");

            let exchange_account = &mut ctx.accounts.exchange_account.load_mut()?;
            let assets_list = &mut ctx.accounts.assets_list.load_mut()?;

            let slot = Clock::get()?.slot;

            // Adjust staking round
            adjust_staking_rounds(&mut self.staking, slot, self.debt_shares);

            // adjust current staking points for exchange account
            adjust_staking_account(exchange_account, &self.staking);

            let user_collateral_account = &mut ctx.accounts.user_collateral_account;

            let tx_signer = ctx.accounts.owner.key;
            // Signer need to be owner of source account
            if !tx_signer.eq(&user_collateral_account.owner) {
                return Err(ErrorCode::InvalidSigner.into());
            }

            let asset = assets_list
                .assets
                .iter_mut()
                .find(|x| {
                    x.collateral.reserve_address.eq(ctx
                        .accounts
                        .reserve_address
                        .to_account_info()
                        .key)
                })
                .unwrap();

            if !asset.collateral.is_collateral {
                return Err(ErrorCode::NotCollateral.into());
            }

            asset.collateral.reserve_balance = asset
                .collateral
                .reserve_balance
                .checked_add(amount)
                .unwrap();

            let exchange_account_collateral = exchange_account.collaterals.iter_mut().find(|x| {
                x.collateral_address
                    .eq(&asset.collateral.collateral_address)
            });

            match exchange_account_collateral {
                Some(entry) => entry.amount = entry.amount.checked_add(amount).unwrap(),
                None => exchange_account.append(CollateralEntry {
                    amount,
                    collateral_address: asset.collateral.collateral_address,
                    index: 0,
                    ..Default::default()
                }),
            }

            // Transfer token
            let seeds = &[SYNTHETIFY_EXCHANGE_SEED.as_bytes(), &[self.nonce]];
            let signer = &[&seeds[..]];
            let cpi_ctx = CpiContext::from(&*ctx.accounts).with_signer(signer);

            token::transfer(cpi_ctx, amount)?;
            Ok(())
        }
        #[access_control(halted(&self)
        version(&self,&ctx.accounts.exchange_account)
        usd_token(&ctx.accounts.usd_token,&ctx.accounts.assets_list)
        assets_list(&self,&ctx.accounts.assets_list))]
        pub fn mint(&mut self, ctx: Context<Mint>, amount: u64) -> Result<()> {
            msg!("Synthetify: MINT");
            let slot = Clock::get()?.slot;

            // Adjust staking round
            adjust_staking_rounds(&mut self.staking, slot, self.debt_shares);

            let exchange_account = &mut ctx.accounts.exchange_account.load_mut()?;
            // adjust current staking points for exchange account
            adjust_staking_account(exchange_account, &self.staking);

            let assets_list = &mut ctx.accounts.assets_list.load_mut()?;

            let total_debt = calculate_debt(assets_list, slot, self.max_delay).unwrap();
            let user_debt =
                calculate_user_debt_in_usd(exchange_account, total_debt, self.debt_shares);
            let max_debt = calculate_max_debt_in_usd(exchange_account, assets_list);

            let assets = &mut assets_list.assets;

            // We can only mint xUSD
            // Both xUSD and collateral token have static index in assets array
            let mint_asset = &assets[0];

            if max_debt < amount.checked_add(user_debt).unwrap().into() {
                return Err(ErrorCode::MintLimit.into());
            }

            // Adjust program and user debt_shares
            // Rounding up - debt is created in favor of the system
            let new_shares =
                calculate_new_shares_by_rounding_up(self.debt_shares, total_debt, amount);
            self.debt_shares = self.debt_shares.checked_add(new_shares).unwrap();
            exchange_account.debt_shares = exchange_account
                .debt_shares
                .checked_add(new_shares)
                .unwrap();
            // Change points for next staking round
            exchange_account.user_staking_data.next_round_points = exchange_account.debt_shares;
            self.staking.next_round.all_points = self.debt_shares;

            let new_supply = mint_asset.synthetic.supply.checked_add(amount).unwrap();
            set_asset_supply(&mut assets[0], new_supply)?;
            let seeds = &[SYNTHETIFY_EXCHANGE_SEED.as_bytes(), &[self.nonce]];
            let signer = &[&seeds[..]];
            // Mint xUSD to user
            let mint_cpi_ctx = CpiContext::from(&*ctx.accounts).with_signer(signer);
            token::mint_to(mint_cpi_ctx, amount)?;
            Ok(())
        }
        #[access_control(halted(&self)
        version(&self,&ctx.accounts.exchange_account)
        assets_list(&self,&ctx.accounts.assets_list))]
        pub fn withdraw(&mut self, ctx: Context<Withdraw>, amount: u64) -> Result<()> {
            msg!("Synthetify: WITHDRAW");

            let slot = Clock::get()?.slot;

            // Adjust staking round
            adjust_staking_rounds(&mut self.staking, slot, self.debt_shares);

            // adjust current staking points for exchange account
            let exchange_account = &mut ctx.accounts.exchange_account.load_mut()?;
            adjust_staking_account(exchange_account, &self.staking);

            // Check signer
            let user_collateral_account = &mut ctx.accounts.user_collateral_account;
            let tx_signer = ctx.accounts.owner.key;
            if !tx_signer.eq(&user_collateral_account.owner) {
                return Err(ErrorCode::InvalidSigner.into());
            }

            // Calculate debt
            let assets_list = &mut ctx.accounts.assets_list.load_mut()?;
            let total_debt = calculate_debt(assets_list, slot, self.max_delay).unwrap();
            let max_debt = calculate_max_debt_in_usd(exchange_account, assets_list);
            let user_debt =
                calculate_user_debt_in_usd(exchange_account, total_debt, self.debt_shares);

            let asset = match assets_list
                .assets
                .iter_mut()
                .find(|x| x.synthetic.asset_address.eq(&user_collateral_account.mint))
            {
                Some(v) => v,
                None => return Err(ErrorCode::NoAssetFound.into()),
            };

            let mut exchange_account_collateral =
                match exchange_account.collaterals.iter_mut().find(|x| {
                    x.collateral_address
                        .eq(&asset.collateral.collateral_address)
                }) {
                    Some(v) => v,
                    None => return Err(ErrorCode::NoAssetFound.into()),
                };

            // Check if not overdrafing
            let max_withdraw_in_usd = calculate_max_withdraw_in_usd(
                max_debt as u64,
                user_debt,
                asset.collateral.collateral_ratio,
            );
            let max_withdrawable = calculate_max_withdrawable(asset, max_withdraw_in_usd);

            if amount > max_withdrawable {
                return Err(ErrorCode::WithdrawLimit.into());
            }

            // Update balance on exchange account
            exchange_account_collateral.amount = exchange_account_collateral
                .amount
                .checked_sub(amount)
                .unwrap();

            // Update reserve balance in AssetList
            asset.collateral.reserve_balance = asset
                .collateral
                .reserve_balance
                .checked_sub(amount)
                .unwrap(); // should never fail

            // Send withdrawn collateral to user
            let seeds = &[SYNTHETIFY_EXCHANGE_SEED.as_bytes(), &[self.nonce]];
            let signer = &[&seeds[..]];
            let cpi_ctx = CpiContext::from(&*ctx.accounts).with_signer(signer);
            token::transfer(cpi_ctx, amount)?;
            Ok(())
        }

        #[access_control(halted(&self)
        version(&self,&ctx.accounts.exchange_account)
        assets_list(&self,&ctx.accounts.assets_list))]
        pub fn swap(&mut self, ctx: Context<Swap>, amount: u64) -> Result<()> {
            msg!("Synthetify: SWAP");

            let slot = Clock::get()?.slot;
            // Adjust staking round
            adjust_staking_rounds(&mut self.staking, slot, self.debt_shares);

            let exchange_account = &mut ctx.accounts.exchange_account.load_mut()?;
            // adjust current staking points for exchange account
            adjust_staking_account(exchange_account, &self.staking);

            let token_address_in = ctx.accounts.token_in.key;
            let token_address_for = ctx.accounts.token_for.key;
            let slot = Clock::get()?.slot;
            let assets_list = &mut ctx.accounts.assets_list.load_mut()?;
            let assets = &mut assets_list.assets;
            let user_token_account_in = &ctx.accounts.user_token_account_in;
            let tx_signer = ctx.accounts.owner.key;

            // Signer need to be owner of source account
            if !tx_signer.eq(&user_token_account_in.owner) {
                return Err(ErrorCode::InvalidSigner.into());
            }
            if token_address_for.eq(&assets[1].synthetic.asset_address) {
                return Err(ErrorCode::SyntheticCollateral.into());
            }
            // Swaping for same assets is forbidden
            if token_address_in.eq(token_address_for) {
                return Err(ErrorCode::WashTrade.into());
            }
            //Get indexes of both assets
            let asset_in_index = assets
                .iter()
                .position(|x| x.synthetic.asset_address == *token_address_in)
                .unwrap();
            let asset_for_index = assets
                .iter()
                .position(|x| x.synthetic.asset_address == *token_address_for)
                .unwrap();

            // Check is oracles have been updated
            check_feed_update(
                assets,
                asset_in_index,
                asset_for_index,
                self.max_delay,
                slot,
            )
            .unwrap();

            let collateral_amount = get_user_sny_collateral_balance(exchange_account, &assets[1]);
            // Get effective_fee base on user collateral balance
            let discount = amount_to_discount(collateral_amount);
            let effective_fee = self
                .fee
                .checked_sub(
                    (self
                        .fee
                        .checked_mul(discount as u32)
                        .unwrap()
                        .checked_div(100))
                    .unwrap(),
                )
                .unwrap();

            // Output amount ~ 100% - fee of input
            let amount_for = calculate_swap_out_amount(
                &assets[asset_in_index],
                &assets[asset_for_index],
                amount,
                effective_fee,
            );
            let seeds = &[SYNTHETIFY_EXCHANGE_SEED.as_bytes(), &[self.nonce]];
            let signer = &[&seeds[..]];

            // Set new supply output token
            let new_supply_output = assets[asset_for_index]
                .synthetic
                .supply
                .checked_add(amount_for)
                .unwrap();
            set_asset_supply(&mut assets[asset_for_index], new_supply_output)?;
            // Set new supply input token
            let new_supply_input = assets[asset_in_index]
                .synthetic
                .supply
                .checked_sub(amount)
                .unwrap();
            set_asset_supply(&mut assets[asset_in_index], new_supply_input)?;
            // Burn input token
            let cpi_ctx_burn: CpiContext<Burn> =
                CpiContext::from(&*ctx.accounts).with_signer(signer);
            token::burn(cpi_ctx_burn, amount)?;

            // Mint output token
            let cpi_ctx_mint: CpiContext<MintTo> =
                CpiContext::from(&*ctx.accounts).with_signer(signer);
            token::mint_to(cpi_ctx_mint, amount_for)?;
            Ok(())
        }
        #[access_control(halted(&self)
        version(&self,&ctx.accounts.exchange_account)
        assets_list(&self,&ctx.accounts.assets_list)
        usd_token(&ctx.accounts.usd_token,&ctx.accounts.assets_list))]
        pub fn burn(&mut self, ctx: Context<BurnToken>, amount: u64) -> Result<()> {
            msg!("Synthetify: BURN");
            let slot = Clock::get()?.slot;

            // Adjust staking round
            adjust_staking_rounds(&mut self.staking, slot, self.debt_shares);

            let exchange_account = &mut ctx.accounts.exchange_account.load_mut()?;
            // adjust current staking points for exchange account
            adjust_staking_account(exchange_account, &self.staking);

            let assets_list = &mut ctx.accounts.assets_list.load_mut()?;
            let debt = calculate_debt(assets_list, slot, self.max_delay).unwrap();

            let assets = &mut assets_list.assets;

            let tx_signer = ctx.accounts.owner.key;
            let user_token_account_burn = &ctx.accounts.user_token_account_burn;

            // Signer need to be owner of source account
            if !tx_signer.eq(&user_token_account_burn.owner) {
                return Err(ErrorCode::InvalidSigner.into());
            }
            // xUSD got static index 0
            let burn_asset = &mut assets[0];

            let user_debt = calculate_user_debt_in_usd(exchange_account, debt, self.debt_shares);

            // Rounding down - debt is burned in favor of the system
            let burned_shares = calculate_burned_shares(
                &burn_asset,
                user_debt,
                exchange_account.debt_shares,
                amount,
            );

            let seeds = &[SYNTHETIFY_EXCHANGE_SEED.as_bytes(), &[self.nonce]];
            let signer = &[&seeds[..]];

            // Check if user burned more than debt
            if burned_shares >= exchange_account.debt_shares {
                // Burn adjusted amount
                let burned_amount = calculate_max_burned_in_xusd(&burn_asset, user_debt);
                self.debt_shares = self
                    .debt_shares
                    .checked_sub(exchange_account.debt_shares)
                    .unwrap();

                self.staking.next_round.all_points = self.debt_shares;
                // Should be fine used checked math just in case
                self.staking.current_round.all_points = self
                    .staking
                    .current_round
                    .all_points
                    .checked_sub(exchange_account.user_staking_data.current_round_points)
                    .unwrap();

                exchange_account.debt_shares = 0;
                // Change points for next staking round
                exchange_account.user_staking_data.next_round_points = 0;
                // Change points for current staking round
                exchange_account.user_staking_data.current_round_points = 0;

                // Change supply
                set_asset_supply(
                    burn_asset,
                    burn_asset
                        .synthetic
                        .supply
                        .checked_sub(burned_amount)
                        .unwrap(),
                )?;
                // Burn token
                // We do not use full allowance maybe its better to burn full allowance
                // and mint matching amount
                let cpi_ctx = CpiContext::from(&*ctx.accounts).with_signer(signer);
                token::burn(cpi_ctx, burned_amount)?;
                Ok(())
            } else {
                // Burn intended amount
                exchange_account.debt_shares = exchange_account
                    .debt_shares
                    .checked_sub(burned_shares)
                    .unwrap();
                self.debt_shares = self.debt_shares.checked_sub(burned_shares).unwrap();
                self.staking.next_round.all_points = self.debt_shares;

                // Change points for next staking round
                exchange_account.user_staking_data.next_round_points = exchange_account.debt_shares;
                // Change points for current staking round
                if exchange_account.user_staking_data.current_round_points >= burned_shares {
                    exchange_account.user_staking_data.current_round_points = exchange_account
                        .user_staking_data
                        .current_round_points
                        .checked_sub(burned_shares)
                        .unwrap();
                    self.staking.current_round.all_points = self
                        .staking
                        .current_round
                        .all_points
                        .checked_sub(burned_shares)
                        .unwrap();
                } else {
                    self.staking.current_round.all_points = self
                        .staking
                        .current_round
                        .all_points
                        .checked_sub(exchange_account.user_staking_data.current_round_points)
                        .unwrap();
                    exchange_account.user_staking_data.current_round_points = 0;
                }

                // Change supply
                set_asset_supply(
                    burn_asset,
                    burn_asset.synthetic.supply.checked_sub(amount).unwrap(),
                )?;
                // Burn token
                let cpi_ctx = CpiContext::from(&*ctx.accounts).with_signer(signer);
                token::burn(cpi_ctx, amount)?;
                Ok(())
            }
        }

        // #[access_control(halted(&self)
        // version(&self,&ctx.accounts.exchange_account)
        // assets_list(&self,&ctx.accounts.assets_list)
        // usd_token(&ctx.accounts.usd_token,&ctx.accounts.assets_list)
        // collateral_account(&self,&ctx.accounts.collateral_account))]
        // pub fn liquidate(&mut self, ctx: Context<Liquidate>) -> Result<()> {
        //     msg!("Synthetify: LIQUIDATE");

        //     let slot = Clock::get()?.slot;

        //     // Adjust staking round
        //     adjust_staking_rounds(&mut self.staking, slot, self.debt_shares);

        //     let exchange_account = &mut ctx.accounts.exchange_account.load_mut()?;
        //     // adjust current staking points for exchange account
        //     adjust_staking_account(exchange_account, &self.staking);

        //     let liquidation_account = ctx.accounts.liquidation_account.to_account_info().key;
        //     let assets_list = &mut ctx.accounts.assets_list.load_mut()?;
        //     let signer = ctx.accounts.signer.key;
        //     let user_usd_account = &ctx.accounts.user_usd_account;
        //     let collateral_account = &ctx.accounts.collateral_account;

        //     let debt = calculate_debt(assets_list, slot, self.max_delay).unwrap();
        //     let assets = &mut assets_list.assets;

        //     // xUSD as collateral_asset have static indexes
        //     let usd_token = &assets[0];
        //     let collateral_asset = &assets[1];

        //     // Signer need to be owner of source amount
        //     if !signer.eq(&user_usd_account.owner) {
        //         return Err(ErrorCode::InvalidSigner.into());
        //     }

        //     // Check program liquidation account
        //     if !liquidation_account.eq(&self.liquidation_account) {
        //         return Err(ErrorCode::ExchangeLiquidationAccount.into());
        //     }

        //     // Time given user to adjust collateral ratio passed
        //     if exchange_account.liquidation_deadline > slot {
        //         return Err(ErrorCode::LiquidationDeadline.into());
        //     }

        //     let collateral_amount_in_token = calculate_user_collateral_in_token(
        //         exchange_account.collateral_shares,
        //         self.collateral_shares,
        //         collateral_account.amount,
        //     );

        //     let collateral_amount_in_usd =
        //         calculate_amount_mint_in_usd(&collateral_asset, collateral_amount_in_token);

        //     let user_debt = calculate_user_debt_in_usd(exchange_account, debt, self.debt_shares);

        //     // Check if collateral ratio is user 200%
        //     check_liquidation(
        //         collateral_amount_in_usd,
        //         user_debt,
        //         self.liquidation_threshold,
        //     )
        //     .unwrap();

        //     let (burned_amount, user_reward_usd, system_reward_usd) = calculate_liquidation(
        //         collateral_amount_in_usd,
        //         user_debt,
        //         self.collateralization_level,
        //         self.liquidation_penalty,
        //     );
        //     // Get amount of collateral send to luquidator and system account
        //     let amount_to_liquidator = usd_to_token_amount(&collateral_asset, user_reward_usd);
        //     let amount_to_system = usd_to_token_amount(&collateral_asset, system_reward_usd);

        //     // Rounding down - debt is burned in favor of the system
        //     let burned_debt_shares =
        //         amount_to_shares_by_rounding_down(self.debt_shares, debt, burned_amount);
        //     // Rounding up - collateral is withdrawn in favor of the system
        //     let burned_collateral_shares = amount_to_shares_by_rounding_up(
        //         self.collateral_shares,
        //         collateral_account.amount,
        //         amount_to_system.checked_add(amount_to_liquidator).unwrap(),
        //     );

        //     // Adjust shares of collateral and debt
        //     self.collateral_shares = self
        //         .collateral_shares
        //         .checked_sub(burned_collateral_shares)
        //         .unwrap();
        //     self.debt_shares = self.debt_shares.checked_sub(burned_debt_shares).unwrap();
        //     exchange_account.debt_shares = exchange_account
        //         .debt_shares
        //         .checked_sub(burned_debt_shares)
        //         .unwrap();
        //     exchange_account.collateral_shares = exchange_account
        //         .collateral_shares
        //         .checked_sub(burned_collateral_shares)
        //         .unwrap();

        //     // Remove staking for liquidation
        //     self.staking.next_round.all_points = self.debt_shares;
        //     self.staking.current_round.all_points = self
        //         .staking
        //         .current_round
        //         .all_points
        //         .checked_sub(exchange_account.user_staking_data.current_round_points)
        //         .unwrap();
        //     self.staking.finished_round.all_points = self
        //         .staking
        //         .finished_round
        //         .all_points
        //         .checked_sub(exchange_account.user_staking_data.finished_round_points)
        //         .unwrap();
        //     exchange_account.user_staking_data.finished_round_points = 0u64;
        //     exchange_account.user_staking_data.current_round_points = 0u64;
        //     exchange_account.user_staking_data.next_round_points = exchange_account.debt_shares;

        //     // Remove liquidation_deadline from liquidated account
        //     exchange_account.liquidation_deadline = u64::MAX;

        //     let seeds = &[SYNTHETIFY_EXCHANGE_SEED.as_bytes(), &[self.nonce]];
        //     let signer_seeds = &[&seeds[..]];
        //     {
        //         // burn xUSD
        //         let new_supply = usd_token.supply.checked_sub(burned_amount).unwrap();
        //         set_asset_supply(&mut assets[0], new_supply)?;
        //         let burn_accounts = Burn {
        //             mint: ctx.accounts.usd_token.to_account_info(),
        //             to: ctx.accounts.user_usd_account.to_account_info(),
        //             authority: ctx.accounts.exchange_authority.to_account_info(),
        //         };
        //         let token_program = ctx.accounts.token_program.to_account_info();
        //         let burn = CpiContext::new(token_program, burn_accounts).with_signer(signer_seeds);
        //         token::burn(burn, burned_amount)?;
        //     }
        //     {
        //         // transfer collateral to liquidator
        //         let liquidator_accounts = Transfer {
        //             from: ctx.accounts.collateral_account.to_account_info(),
        //             to: ctx.accounts.user_collateral_account.to_account_info(),
        //             authority: ctx.accounts.exchange_authority.to_account_info(),
        //         };
        //         let token_program = ctx.accounts.token_program.to_account_info();
        //         let transfer =
        //             CpiContext::new(token_program, liquidator_accounts).with_signer(signer_seeds);
        //         token::transfer(transfer, amount_to_liquidator)?;
        //     }
        //     {
        //         // transfer collateral to liquidation_account
        //         let system_accounts = Transfer {
        //             from: ctx.accounts.collateral_account.to_account_info(),
        //             to: ctx.accounts.liquidation_account.to_account_info(),
        //             authority: ctx.accounts.exchange_authority.to_account_info(),
        //         };
        //         let token_program = ctx.accounts.token_program.to_account_info();
        //         let transfer =
        //             CpiContext::new(token_program, system_accounts).with_signer(signer_seeds);
        //         token::transfer(transfer, amount_to_system)?;
        //     }

        //     Ok(())
        // }
        // #[access_control(halted(&self)
        // version(&self,&ctx.accounts.exchange_account)
        // collateral_account(&self,&ctx.accounts.collateral_account)
        // assets_list(&self,&ctx.accounts.assets_list))]
        // pub fn check_account_collateralization(
        //     &mut self,
        //     ctx: Context<CheckCollateralization>,
        // ) -> Result<()> {
        //     msg!("Synthetify: CHECK ACCOUNT COLLATERALIZATION");

        //     let slot = Clock::get()?.slot;

        //     // Adjust staking round
        //     adjust_staking_rounds(&mut self.staking, slot, self.debt_shares);

        //     let exchange_account = &mut ctx.accounts.exchange_account.load_mut()?;
        //     // adjust current staking points for exchange account
        //     adjust_staking_account(exchange_account, &self.staking);

        //     let assets_list = &ctx.accounts.assets_list.load_mut()?;
        //     let collateral_account = &ctx.accounts.collateral_account;

        //     let assets = &assets_list.assets;
        //     let collateral_asset = &assets[1];

        //     let collateral_amount_in_token = calculate_user_collateral_in_token(
        //         exchange_account.collateral_shares,
        //         self.collateral_shares,
        //         collateral_account.amount,
        //     );
        //     let collateral_amount_in_usd =
        //         calculate_amount_mint_in_usd(&collateral_asset, collateral_amount_in_token);

        //     let debt = calculate_debt(assets_list, slot, self.max_delay).unwrap();
        //     let user_debt = calculate_user_debt_in_usd(exchange_account, debt, self.debt_shares);

        //     let result = check_liquidation(
        //         collateral_amount_in_usd,
        //         user_debt,
        //         self.liquidation_threshold,
        //     );
        //     // If account is undercollaterized set liquidation_deadline
        //     // After liquidation_deadline slot account can be liquidated
        //     match result {
        //         Ok(_) => {
        //             if exchange_account.liquidation_deadline == u64::MAX {
        //                 exchange_account.liquidation_deadline =
        //                     slot.checked_add(self.liquidation_buffer.into()).unwrap();
        //             }
        //         }
        //         Err(_) => {
        //             exchange_account.liquidation_deadline = u64::MAX;
        //         }
        //     }

        //     Ok(())
        // }

        // #[access_control(halted(&self) version(&self,&ctx.accounts.exchange_account))]
        // pub fn claim_rewards(&mut self, ctx: Context<ClaimRewards>) -> Result<()> {
        //     msg!("Synthetify: CLAIM REWARDS");

        //     let slot = Clock::get()?.slot;

        //     // Adjust staking round
        //     adjust_staking_rounds(&mut self.staking, slot, self.debt_shares);
        //     let exchange_account = &mut ctx.accounts.exchange_account.load_mut()?;

        //     // adjust current staking points for exchange account
        //     adjust_staking_account(exchange_account, &self.staking);

        //     if self.staking.finished_round.amount > 0 {
        //         let reward_amount = self
        //             .staking
        //             .finished_round
        //             .amount
        //             .checked_mul(exchange_account.user_staking_data.finished_round_points)
        //             .unwrap()
        //             .checked_div(self.staking.finished_round.all_points)
        //             .unwrap();

        //         exchange_account.user_staking_data.amount_to_claim = exchange_account
        //             .user_staking_data
        //             .amount_to_claim
        //             .checked_add(reward_amount)
        //             .unwrap();
        //         exchange_account.user_staking_data.finished_round_points = 0;
        //     }

        //     Ok(())
        // }
        // #[access_control(halted(&self)
        // version(&self,&ctx.accounts.exchange_account)
        // fund_account(&self,&ctx.accounts.staking_fund_account))]
        // pub fn withdraw_rewards(&mut self, ctx: Context<WithdrawRewards>) -> Result<()> {
        //     msg!("Synthetify: WITHDRAW REWARDS");

        //     let slot = Clock::get()?.slot;
        //     // Adjust staking round
        //     adjust_staking_rounds(&mut self.staking, slot, self.debt_shares);

        //     let exchange_account = &mut ctx.accounts.exchange_account.load_mut()?;
        //     // adjust current staking points for exchange account
        //     adjust_staking_account(exchange_account, &self.staking);

        //     if exchange_account.user_staking_data.amount_to_claim == 0u64 {
        //         return Err(ErrorCode::NoRewards.into());
        //     }
        //     let seeds = &[SYNTHETIFY_EXCHANGE_SEED.as_bytes(), &[self.nonce]];
        //     let signer_seeds = &[&seeds[..]];

        //     // Transfer rewards
        //     let cpi_accounts = Transfer {
        //         from: ctx.accounts.staking_fund_account.to_account_info(),
        //         to: ctx.accounts.user_token_account.to_account_info(),
        //         authority: ctx.accounts.exchange_authority.to_account_info(),
        //     };
        //     let cpi_program = ctx.accounts.token_program.to_account_info();
        //     let cpi_ctx = CpiContext::new(cpi_program, cpi_accounts).with_signer(signer_seeds);
        //     token::transfer(cpi_ctx, exchange_account.user_staking_data.amount_to_claim)?;
        //     // Reset rewards amount
        //     exchange_account.user_staking_data.amount_to_claim = 0u64;
        //     Ok(())
        // }
        // #[access_control(halted(&self))]
        // pub fn withdraw_liquidation_penalty(
        //     &mut self,
        //     ctx: Context<WithdrawLiquidationPenalty>,
        //     amount: u64,
        // ) -> Result<()> {
        //     msg!("Synthetify: WITHDRAW LIQUIDATION PENALTY");

        //     if !ctx.accounts.admin.key.eq(&self.admin) {
        //         return Err(ErrorCode::Unauthorized.into());
        //     }
        //     if !ctx
        //         .accounts
        //         .liquidation_account
        //         .to_account_info()
        //         .key
        //         .eq(&self.liquidation_account)
        //     {
        //         return Err(ErrorCode::ExchangeLiquidationAccount.into());
        //     }
        //     let seeds = &[SYNTHETIFY_EXCHANGE_SEED.as_bytes(), &[self.nonce]];
        //     let signer_seeds = &[&seeds[..]];

        //     // Transfer
        //     let cpi_accounts = Transfer {
        //         from: ctx.accounts.liquidation_account.to_account_info(),
        //         to: ctx.accounts.to.to_account_info(),
        //         authority: ctx.accounts.exchange_authority.to_account_info(),
        //     };
        //     let cpi_program = ctx.accounts.token_program.to_account_info();
        //     let cpi_ctx = CpiContext::new(cpi_program, cpi_accounts).with_signer(signer_seeds);
        //     token::transfer(cpi_ctx, amount)?;
        //     Ok(())
        // }
        // admin methods
        #[access_control(admin(&self, &ctx.accounts.admin))]
        pub fn set_liquidation_buffer(
            &mut self,
            ctx: Context<AdminAction>,
            liquidation_buffer: u32,
        ) -> Result<()> {
            msg!("Synthetify:Admin: SET LIQUIDATION BUFFER");

            self.liquidation_buffer = liquidation_buffer;
            Ok(())
        }
        #[access_control(admin(&self, &ctx.accounts.admin))]
        pub fn set_liquidation_threshold(
            &mut self,
            ctx: Context<AdminAction>,
            liquidation_threshold: u8,
        ) -> Result<()> {
            msg!("Synthetify:Admin: SET LIQUIDATION THRESHOLD");

            self.liquidation_threshold = liquidation_threshold;
            Ok(())
        }
        #[access_control(admin(&self, &ctx.accounts.admin))]
        pub fn set_liquidation_penalty(
            &mut self,
            ctx: Context<AdminAction>,
            liquidation_penalty: u8,
        ) -> Result<()> {
            msg!("Synthetify:Admin: SET LIQUIDATION PENALTY");

            self.liquidation_penalty = liquidation_penalty;
            Ok(())
        }
        #[access_control(admin(&self, &ctx.accounts.admin))]
        pub fn set_collateralization_level(
            &mut self,
            ctx: Context<AdminAction>,
            collateralization_level: u32,
        ) -> Result<()> {
            msg!("Synthetify:Admin: SET COLLATERALIZATION LEVEL");

            self.collateralization_level = collateralization_level;
            Ok(())
        }
        #[access_control(admin(&self, &ctx.accounts.admin))]
        pub fn set_fee(&mut self, ctx: Context<AdminAction>, fee: u32) -> Result<()> {
            msg!("Synthetify:Admin: SET FEE");

            self.fee = fee;
            Ok(())
        }
        #[access_control(admin(&self, &ctx.accounts.admin))]
        pub fn set_max_delay(&mut self, ctx: Context<AdminAction>, max_delay: u32) -> Result<()> {
            msg!("Synthetify:Admin: SET MAX DELAY");

            self.max_delay = max_delay;
            Ok(())
        }
        #[access_control(admin(&self, &ctx.accounts.admin))]
        pub fn set_halted(&mut self, ctx: Context<AdminAction>, halted: bool) -> Result<()> {
            msg!("Synthetify:Admin: SET HALTED");

            self.halted = halted;
            Ok(())
        }
        #[access_control(admin(&self, &ctx.accounts.admin))]
        pub fn set_staking_amount_per_round(
            &mut self,
            ctx: Context<AdminAction>,
            amount_per_round: u64,
        ) -> Result<()> {
            msg!("Synthetify:Admin:Staking: SET AMOUNT PER ROUND");

            self.staking.amount_per_round = amount_per_round;
            Ok(())
        }
        #[access_control(admin(&self, &ctx.accounts.admin))]
        pub fn set_staking_round_length(
            &mut self,
            ctx: Context<AdminAction>,
            round_length: u32,
        ) -> Result<()> {
            msg!("Synthetify:Admin:Staking: SET ROUND LENGTH");

            self.staking.round_length = round_length;
            Ok(())
        }
        #[access_control(admin(&self, &ctx.accounts.signer))]
        pub fn add_new_asset(
            &mut self,
            ctx: Context<AddNewAsset>,
            new_asset_feed_address: Pubkey,
            new_asset_address: Pubkey,
            new_asset_decimals: u8,
            new_asset_max_supply: u64,
        ) -> Result<()> {
            let mut assets_list = ctx.accounts.assets_list.load_mut()?;
            if !assets_list.initialized {
                return Err(ErrorCode::Uninitialized.into());
            }
            let new_asset = Asset {
                feed_address: new_asset_feed_address,
                last_update: 0,
                price: 0,
                confidence: 0,
                synthetic: Synthetic {
                    decimals: new_asset_decimals,
                    asset_address: new_asset_address,
                    supply: 0,
                    max_supply: new_asset_max_supply,
                    settlement_slot: u64::MAX,
                },
                collateral: Collateral {
                    is_collateral: false,
                    ..Default::default()
                },
            };

            assets_list.append(new_asset);
            Ok(())
        }
        #[access_control(admin(&self, &ctx.accounts.signer))]
        pub fn set_max_supply(
            &mut self,
            ctx: Context<SetMaxSupply>,
            asset_address: Pubkey,
            new_max_supply: u64,
        ) -> Result<()> {
            let mut assets_list = ctx.accounts.assets_list.load_mut()?;

            let asset = assets_list
                .assets
                .iter_mut()
                .find(|x| x.synthetic.asset_address == asset_address);

            match asset {
                Some(asset) => asset.synthetic.max_supply = new_max_supply,
                None => return Err(ErrorCode::NoAssetFound.into()),
            }
            Ok(())
        }
        #[access_control(admin(&self, &ctx.accounts.signer))]
        pub fn set_price_feed(
            &mut self,
            ctx: Context<SetPriceFeed>,
            asset_address: Pubkey,
        ) -> Result<()> {
            let mut assets_list = ctx.accounts.assets_list.load_mut()?;

            let asset = assets_list
                .assets
                .iter_mut()
                .find(|x| x.synthetic.asset_address == asset_address);

            match asset {
                Some(asset) => asset.feed_address = *ctx.accounts.price_feed.key,
                None => return Err(ErrorCode::NoAssetFound.into()),
            }
            Ok(())
        }
    }
    pub fn create_exchange_account(
        ctx: Context<CreateExchangeAccount>,
        owner: Pubkey,
    ) -> ProgramResult {
        let exchange_account = &mut ctx.accounts.exchange_account.load_init()?;
        exchange_account.owner = owner;
        exchange_account.debt_shares = 0;
        exchange_account.version = 0;
        exchange_account.liquidation_deadline = u64::MAX;
        exchange_account.user_staking_data = UserStaking::default();
        Ok(())
    }
    pub fn create_assets_list(ctx: Context<CreateAssetsList>) -> ProgramResult {
        let assets_list = &mut ctx.accounts.assets_list.load_init()?;
        assets_list.initialized = false;
        Ok(())
    }
    // #[access_control(admin(&self, &ctx.accounts.signer))]
    pub fn create_list(
        ctx: Context<InitializeAssetsList>,
        collateral_token: Pubkey,
        collateral_token_feed: Pubkey,

        usd_token: Pubkey,
    ) -> Result<()> {
        let assets_list = &mut ctx.accounts.assets_list.load_mut()?;

        if assets_list.initialized {
            return Err(ErrorCode::Initialized.into());
        }
        let usd_asset = Asset {
            feed_address: Pubkey::default(), // unused
            last_update: u64::MAX,           // we dont update usd price
            price: 1 * 10u64.pow(PRICE_OFFSET.into()),
            confidence: 0,
            synthetic: Synthetic {
                decimals: 6,
                asset_address: usd_token,
                supply: 0,
                max_supply: u64::MAX, // no limit for usd asset
                settlement_slot: u64::MAX,
            },
            collateral: Collateral {
                is_collateral: false,
                ..Default::default()
            },
        };
        let collateral_asset = Asset {
            feed_address: collateral_token_feed,
            last_update: 0,
            price: 0,
            confidence: 0,
            synthetic: Synthetic {
                decimals: 6,
                asset_address: collateral_token,
                supply: 0,
                max_supply: 0,
                settlement_slot: u64::MAX,
            },
            collateral: Collateral {
                is_collateral: true,
                collateral_ratio: 10,
                collateral_address: collateral_token,
                reserve_balance: 0,
                decimals: 6,
                reserve_address: *ctx.accounts.reserve_account.key,
            },
        };
        assets_list.append(usd_asset);
        assets_list.append(collateral_asset);
        assets_list.initialized = true;
        Ok(())
    }
    pub fn set_assets_prices(ctx: Context<SetAssetsPrices>) -> Result<()> {
        msg!("SYNTHETIFY: SET ASSETS PRICES");
        let assets_list = &mut ctx.accounts.assets_list.load_mut()?;
        for oracle_account in ctx.remaining_accounts {
            let price_feed = Price::load(oracle_account)?;
            let feed_address = oracle_account.key;
            let asset = assets_list
                .assets
                .iter_mut()
                .find(|x| x.feed_address == *feed_address);
            match asset {
                Some(asset) => {
                    let offset = (PRICE_OFFSET as i32).checked_add(price_feed.expo).unwrap();
                    if offset >= 0 {
                        let scaled_price = price_feed
                            .agg
                            .price
                            .checked_mul(10i64.pow(offset.try_into().unwrap()))
                            .unwrap();

                        asset.price = scaled_price.try_into().unwrap();
                    } else {
                        let scaled_price = price_feed
                            .agg
                            .price
                            .checked_div(10i64.pow((-offset).try_into().unwrap()))
                            .unwrap();

                        asset.price = scaled_price.try_into().unwrap();
                    }

                    asset.confidence =
                        math::calculate_confidence(price_feed.agg.conf, price_feed.agg.price);
                    asset.last_update = Clock::get()?.slot;
                }
                None => return Err(ErrorCode::NoAssetFound.into()),
            }
        }
        Ok(())
    }
}
#[account(zero_copy)]
#[derive(Default)]
pub struct AssetsList {
    pub initialized: bool,
    pub head: u8,
    pub assets: [Asset; 30],
}
impl AssetsList {
    fn append(&mut self, msg: Asset) {
        self.assets[(self.head) as usize] = msg;
        self.head += 1;
    }
}
#[derive(Accounts)]
pub struct CreateAssetsList<'info> {
    #[account(init)]
    pub assets_list: Loader<'info, AssetsList>,
    pub rent: Sysvar<'info, Rent>,
}
#[derive(Accounts)]
pub struct InitializeAssetsList<'info> {
    #[account(mut)]
    pub assets_list: Loader<'info, AssetsList>,
    pub reserve_account: AccountInfo<'info>,
}
#[derive(Accounts)]
pub struct SetAssetsPrices<'info> {
    #[account(mut)]
    pub assets_list: Loader<'info, AssetsList>,
}
#[derive(Accounts)]
pub struct AddNewAsset<'info> {
    #[account(signer)]
    pub signer: AccountInfo<'info>,
    #[account(mut)]
    pub assets_list: Loader<'info, AssetsList>,
}
#[derive(Accounts)]
pub struct SetMaxSupply<'info> {
    #[account(signer)]
    pub signer: AccountInfo<'info>,
    #[account(mut)]
    pub assets_list: Loader<'info, AssetsList>,
}
#[derive(Accounts)]
pub struct SetPriceFeed<'info> {
    #[account(signer)]
    pub signer: AccountInfo<'info>,
    #[account(mut)]
    pub assets_list: Loader<'info, AssetsList>,
    pub price_feed: AccountInfo<'info>,
}
#[derive(Accounts)]
pub struct New<'info> {
    pub admin: AccountInfo<'info>,
    pub assets_list: AccountInfo<'info>,
    pub liquidation_account: AccountInfo<'info>,
    pub staking_fund_account: CpiAccount<'info, TokenAccount>,
}
#[derive(Accounts)]
pub struct CreateExchangeAccount<'info> {
    #[account(init,associated = admin, with = state,payer=payer)]
    pub exchange_account: Loader<'info, ExchangeAccount>,
    pub admin: AccountInfo<'info>,
    #[account(mut, signer)]
    pub payer: AccountInfo<'info>,
    pub rent: Sysvar<'info, Rent>,
    pub state: Loader<'info, InternalState>,
    pub system_program: AccountInfo<'info>,
}

#[associated(zero_copy)]
#[derive(PartialEq, Default, Debug)]
pub struct ExchangeAccount {
    pub owner: Pubkey,                  // Identity controling account
    pub version: u8,                    // Version of account struct
    pub debt_shares: u64,               // Shares representing part of entire debt pool
    pub liquidation_deadline: u64,      // Slot number after which account can be liquidated
    pub user_staking_data: UserStaking, // Staking information
    pub head: u8,
    pub collaterals: [CollateralEntry; 10],
}
#[zero_copy]
#[derive(PartialEq, Default, Debug)]
pub struct CollateralEntry {
    amount: u64,
    collateral_address: Pubkey,
    index: u8, // index could be usefull to quickly find asset in list
}
impl ExchangeAccount {
    fn append(&mut self, entry: CollateralEntry) {
        self.collaterals[(self.head) as usize] = entry;
        self.head += 1;
    }
}
#[derive(Accounts)]
pub struct Withdraw<'info> {
    #[account(mut)]
    pub assets_list: Loader<'info, AssetsList>,
    pub exchange_authority: AccountInfo<'info>,
    #[account(mut)]
    pub reserve_account: CpiAccount<'info, TokenAccount>,
    #[account(mut)]
    pub user_collateral_account: CpiAccount<'info, TokenAccount>,
    #[account("token_program.key == &token::ID")]
    pub token_program: AccountInfo<'info>,
    #[account(mut, has_one = owner)]
    pub exchange_account: Loader<'info, ExchangeAccount>,
    #[account(signer)]
    pub owner: AccountInfo<'info>,
}
impl<'a, 'b, 'c, 'info> From<&Withdraw<'info>> for CpiContext<'a, 'b, 'c, 'info, Transfer<'info>> {
    fn from(accounts: &Withdraw<'info>) -> CpiContext<'a, 'b, 'c, 'info, Transfer<'info>> {
        let cpi_accounts = Transfer {
            from: accounts.reserve_account.to_account_info(),
            to: accounts.user_collateral_account.to_account_info(),
            authority: accounts.exchange_authority.to_account_info(),
        };
        let cpi_program = accounts.token_program.to_account_info();
        CpiContext::new(cpi_program, cpi_accounts)
    }
}
#[derive(Accounts)]
pub struct Mint<'info> {
    #[account(mut)]
    pub assets_list: Loader<'info, AssetsList>,
    pub exchange_authority: AccountInfo<'info>,
    #[account(mut)]
    pub usd_token: AccountInfo<'info>,
    #[account(mut)]
    pub to: AccountInfo<'info>,
    #[account("token_program.key == &token::ID")]
    pub token_program: AccountInfo<'info>,
    #[account(mut, has_one = owner)]
    pub exchange_account: Loader<'info, ExchangeAccount>,
    #[account(signer)]
    pub owner: AccountInfo<'info>,
}
impl<'a, 'b, 'c, 'info> From<&Mint<'info>> for CpiContext<'a, 'b, 'c, 'info, MintTo<'info>> {
    fn from(accounts: &Mint<'info>) -> CpiContext<'a, 'b, 'c, 'info, MintTo<'info>> {
        let cpi_accounts = MintTo {
            mint: accounts.usd_token.to_account_info(),
            to: accounts.to.to_account_info(),
            authority: accounts.exchange_authority.to_account_info(),
        };
        let cpi_program = accounts.token_program.to_account_info();
        CpiContext::new(cpi_program, cpi_accounts)
    }
}
#[derive(Accounts)]
pub struct Deposit<'info> {
    #[account(mut)]
    pub exchange_account: Loader<'info, ExchangeAccount>,
    #[account(mut)]
    pub reserve_address: CpiAccount<'info, TokenAccount>,
    #[account(mut)]
    pub user_collateral_account: CpiAccount<'info, TokenAccount>,
    #[account("token_program.key == &token::ID")]
    pub token_program: AccountInfo<'info>,
    #[account(mut)]
    pub assets_list: Loader<'info, AssetsList>,
    // owner can deposit to any exchange_account
    #[account(signer)]
    pub owner: AccountInfo<'info>,
    pub exchange_authority: AccountInfo<'info>,
}
impl<'a, 'b, 'c, 'info> From<&Deposit<'info>> for CpiContext<'a, 'b, 'c, 'info, Transfer<'info>> {
    fn from(accounts: &Deposit<'info>) -> CpiContext<'a, 'b, 'c, 'info, Transfer<'info>> {
        let cpi_accounts = Transfer {
            from: accounts.user_collateral_account.to_account_info(),
            to: accounts.reserve_address.to_account_info(),
            authority: accounts.exchange_authority.to_account_info(),
        };
        let cpi_program = accounts.token_program.to_account_info();
        CpiContext::new(cpi_program, cpi_accounts)
    }
}
#[derive(Accounts)]
pub struct Liquidate<'info> {
    pub exchange_authority: AccountInfo<'info>,
    #[account(mut)]
    pub assets_list: Loader<'info, AssetsList>,
    #[account("token_program.key == &token::ID")]
    pub token_program: AccountInfo<'info>,
    #[account(mut)]
    pub usd_token: AccountInfo<'info>,
    #[account(mut)]
    pub user_usd_account: CpiAccount<'info, TokenAccount>,
    #[account(mut)]
    pub user_collateral_account: AccountInfo<'info>,
    #[account(mut)]
    pub exchange_account: Loader<'info, ExchangeAccount>,
    #[account(signer)]
    pub signer: AccountInfo<'info>,
    #[account(mut)]
    pub collateral_account: CpiAccount<'info, TokenAccount>,
    #[account(mut)]
    pub liquidation_account: CpiAccount<'info, TokenAccount>,
}
#[derive(Accounts)]
pub struct BurnToken<'info> {
    pub exchange_authority: AccountInfo<'info>,
    #[account(mut)]
    pub assets_list: Loader<'info, AssetsList>,
    #[account("token_program.key == &token::ID")]
    pub token_program: AccountInfo<'info>,
    #[account(mut)]
    pub usd_token: AccountInfo<'info>,
    #[account(mut)]
    pub user_token_account_burn: CpiAccount<'info, TokenAccount>,
    #[account(mut, has_one = owner)]
    pub exchange_account: Loader<'info, ExchangeAccount>,
    #[account(signer)]
    pub owner: AccountInfo<'info>,
}
impl<'a, 'b, 'c, 'info> From<&BurnToken<'info>> for CpiContext<'a, 'b, 'c, 'info, Burn<'info>> {
    fn from(accounts: &BurnToken<'info>) -> CpiContext<'a, 'b, 'c, 'info, Burn<'info>> {
        let cpi_accounts = Burn {
            mint: accounts.usd_token.to_account_info(),
            to: accounts.user_token_account_burn.to_account_info(),
            authority: accounts.exchange_authority.to_account_info(),
        };
        let cpi_program = accounts.token_program.to_account_info();
        CpiContext::new(cpi_program, cpi_accounts)
    }
}
#[derive(Accounts)]
pub struct Swap<'info> {
    pub exchange_authority: AccountInfo<'info>,
    #[account(mut)]
    pub assets_list: Loader<'info, AssetsList>,
    #[account("token_program.key == &token::ID")]
    pub token_program: AccountInfo<'info>,
    #[account(mut)]
    pub token_in: AccountInfo<'info>,
    #[account(mut)]
    pub token_for: AccountInfo<'info>,
    #[account(mut)]
    pub user_token_account_in: CpiAccount<'info, TokenAccount>,
    #[account(mut)]
    pub user_token_account_for: AccountInfo<'info>,
    #[account(mut, has_one = owner)]
    pub exchange_account: Loader<'info, ExchangeAccount>,
    #[account(signer)]
    pub owner: AccountInfo<'info>,
}
impl<'a, 'b, 'c, 'info> From<&Swap<'info>> for CpiContext<'a, 'b, 'c, 'info, Burn<'info>> {
    fn from(accounts: &Swap<'info>) -> CpiContext<'a, 'b, 'c, 'info, Burn<'info>> {
        let cpi_accounts = Burn {
            mint: accounts.token_in.to_account_info(),
            to: accounts.user_token_account_in.to_account_info(),
            authority: accounts.exchange_authority.to_account_info(),
        };
        let cpi_program = accounts.token_program.to_account_info();
        CpiContext::new(cpi_program, cpi_accounts)
    }
}
impl<'a, 'b, 'c, 'info> From<&Swap<'info>> for CpiContext<'a, 'b, 'c, 'info, MintTo<'info>> {
    fn from(accounts: &Swap<'info>) -> CpiContext<'a, 'b, 'c, 'info, MintTo<'info>> {
        let cpi_accounts = MintTo {
            mint: accounts.token_for.to_account_info(),
            to: accounts.user_token_account_for.to_account_info(),
            authority: accounts.exchange_authority.to_account_info(),
        };
        let cpi_program = accounts.token_program.to_account_info();
        CpiContext::new(cpi_program, cpi_accounts)
    }
}

#[derive(Accounts)]
pub struct CheckCollateralization<'info> {
    #[account(mut)]
    pub exchange_account: Loader<'info, ExchangeAccount>,
    pub assets_list: Loader<'info, AssetsList>,
    pub collateral_account: CpiAccount<'info, TokenAccount>,
}
#[derive(Accounts)]
pub struct ClaimRewards<'info> {
    #[account(mut)]
    pub exchange_account: Loader<'info, ExchangeAccount>,
}
#[derive(Accounts)]
pub struct WithdrawRewards<'info> {
    #[account(mut, has_one = owner)]
    pub exchange_account: Loader<'info, ExchangeAccount>,
    #[account(signer)]
    pub owner: AccountInfo<'info>,
    pub exchange_authority: AccountInfo<'info>,
    #[account("token_program.key == &token::ID")]
    pub token_program: AccountInfo<'info>,
    #[account(mut)]
    pub user_token_account: CpiAccount<'info, TokenAccount>,
    #[account(mut)]
    pub staking_fund_account: CpiAccount<'info, TokenAccount>,
}
#[derive(Accounts)]
pub struct WithdrawLiquidationPenalty<'info> {
    #[account(signer)]
    pub admin: AccountInfo<'info>,
    pub exchange_authority: AccountInfo<'info>,
    #[account("token_program.key == &token::ID")]
    pub token_program: AccountInfo<'info>,
    #[account(mut)]
    pub to: CpiAccount<'info, TokenAccount>,
    #[account(mut)]
    pub liquidation_account: CpiAccount<'info, TokenAccount>,
}
#[derive(Accounts)]
pub struct AdminAction<'info> {
    #[account(signer)]
    pub admin: AccountInfo<'info>,
}
#[zero_copy]
#[derive(PartialEq, Default, Debug)]
pub struct StakingRound {
    pub start: u64,      // 8 Slot when round starts
    pub amount: u64,     // 8 Amount of SNY distributed in this round
    pub all_points: u64, // 8 All points used to calculate user share in staking rewards
}
#[zero_copy]
#[derive(PartialEq, Default, Debug)]
pub struct Staking {
    pub fund_account: Pubkey,         //32 Source account of SNY tokens
    pub round_length: u32,            //4 Length of round in slots
    pub amount_per_round: u64,        //8 Amount of SNY distributed per round
    pub finished_round: StakingRound, //24
    pub current_round: StakingRound,  //24
    pub next_round: StakingRound,     //24
}
#[zero_copy]
#[derive(PartialEq, Default, Debug)]
pub struct UserStaking {
    pub amount_to_claim: u64,       //8 Amount of SNY accumulated by account
    pub finished_round_points: u64, //8 Points are based on debt_shares in specific round
    pub current_round_points: u64,  //8
    pub next_round_points: u64,     //8
    pub last_update: u64,           //8
}
#[zero_copy]
#[derive(PartialEq, Default, Debug)]
pub struct Asset {
    // Synthetic values
    pub feed_address: Pubkey, // 32 Pyth oracle account address
    pub price: u64,           // 8
    pub last_update: u64,     // 8
    pub confidence: u32,      // 4 unused
    pub synthetic: Synthetic,
    pub collateral: Collateral, // Collateral values
}
#[zero_copy]
#[derive(PartialEq, Default, Debug)]
pub struct Collateral {
    pub is_collateral: bool,        // 1
    pub collateral_address: Pubkey, // 32
    pub reserve_address: Pubkey,    // 32
    pub reserve_balance: u64,       // 8
    pub decimals: u8,               // 1
    pub collateral_ratio: u8,       // 1 in %
}
#[zero_copy]
#[derive(PartialEq, Default, Debug)]
pub struct Synthetic {
    pub asset_address: Pubkey, // 32
    pub supply: u64,           // 8
    pub decimals: u8,          // 1
    pub max_supply: u64,       // 8
    pub settlement_slot: u64,  // 8 unused
}
#[error]
pub enum ErrorCode {
    #[msg("You are not admin")]
    Unauthorized,
    #[msg("Not synthetic USD asset")]
    NotSyntheticUsd,
    #[msg("Oracle price is outdated")]
    OutdatedOracle,
    #[msg("Mint limit")]
    MintLimit,
    #[msg("Withdraw limit")]
    WithdrawLimit,
    #[msg("Invalid collateral_account")]
    CollateralAccountError,
    #[msg("Synthetic collateral is not supported")]
    SyntheticCollateral,
    #[msg("Invalid Assets List")]
    InvalidAssetsList,
    #[msg("Invalid Liquidation")]
    InvalidLiquidation,
    #[msg("Invalid signer")]
    InvalidSigner,
    #[msg("Wash trade")]
    WashTrade,
    #[msg("Invalid exchange liquidation account")]
    ExchangeLiquidationAccount,
    #[msg("Liquidation deadline not passed")]
    LiquidationDeadline,
    #[msg("Program is currently Halted")]
    Halted,
    #[msg("No rewards to claim")]
    NoRewards,
    #[msg("Invalid fund_account")]
    FundAccountError,
    #[msg("Invalid version of user account")]
    AccountVersion,
    #[msg("Assets list already initialized")]
    Initialized,
    #[msg("Assets list is not initialized")]
    Uninitialized,
    #[msg("No asset with such address was found")]
    NoAssetFound,
    #[msg("Asset max_supply crossed")]
    MaxSupply,
    #[msg("Asset is not collateral")]
    NotCollateral,
}

// Access control modifiers.

// Only admin access
fn admin<'info>(state: &InternalState, signer: &AccountInfo<'info>) -> Result<()> {
    if !signer.key.eq(&state.admin) {
        return Err(ErrorCode::Unauthorized.into());
    }
    Ok(())
}
// Check if program is halted
fn halted<'info>(state: &InternalState) -> Result<()> {
    if state.halted {
        return Err(ErrorCode::Halted.into());
    }
    Ok(())
}
// Assert right assets_list
fn assets_list<'info>(
    state: &InternalState,
    assets_list: &Loader<'info, AssetsList>,
) -> Result<()> {
    if !assets_list.to_account_info().key.eq(&state.assets_list) {
        return Err(ErrorCode::InvalidAssetsList.into());
    }
    Ok(())
}
// Assert right collateral_account
fn collateral_account<'info>(
    state: &InternalState,
    collateral_account: &CpiAccount<'info, TokenAccount>,
) -> Result<()> {
    if !collateral_account
        .to_account_info()
        .key
        .eq(&state.collateral_account)
    {
        return Err(ErrorCode::CollateralAccountError.into());
    }
    Ok(())
}
// Assert right usd_token
fn usd_token<'info>(usd_token: &AccountInfo, assets_list: &Loader<AssetsList>) -> Result<()> {
    if !usd_token
        .to_account_info()
        .key
        .eq(&assets_list.load()?.assets[0].synthetic.asset_address)
    {
        return Err(ErrorCode::NotSyntheticUsd.into());
    }
    Ok(())
}

// Assert right fundAccount
fn fund_account<'info>(
    state: &InternalState,
    fund_account: &CpiAccount<'info, TokenAccount>,
) -> Result<()> {
    if !fund_account
        .to_account_info()
        .key
        .eq(&state.staking.fund_account)
    {
        return Err(ErrorCode::FundAccountError.into());
    }
    Ok(())
}
// Check is user account have correct version
fn version<'info>(
    state: &InternalState,
    exchange_account: &Loader<'info, ExchangeAccount>,
) -> Result<()> {
    if !exchange_account.load()?.version == state.account_version {
        return Err(ErrorCode::AccountVersion.into());
    }
    Ok(())
}
